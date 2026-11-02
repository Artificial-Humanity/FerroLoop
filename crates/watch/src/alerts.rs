//! Alert thresholds and the pure alert evaluation.
//!
//! [`evaluate_watch_alerts`] takes everything it needs as arguments, the clock included, so the
//! same input always gives the same alerts in the same order: most severe first, then by id.
//! An alert id is built from its rule and the entity it is about (`spend:project:<iri>`,
//! `quota:claude:primary`, `heartbeat_dead:agent:ada`, `crash_loop:record:<iri>`), so a caller
//! can track one alert across polls.
//!
//! # Default thresholds
//!
//! | field | default | meaning |
//! |---|---|---|
//! | `warn_quota_pct` | 80.0 | a vendor window at or above this percentage is a warning |
//! | `critical_quota_pct` | 95.0 | ... and at or above this, critical |
//! | `warn_cost_usd_micros` | 100_000_000 ($100) | a project's (or the workspace's) recorded attempt spend at or above this is a warning |
//! | `critical_cost_usd_micros` | 500_000_000 ($500) | ... and at or above this, critical |
//! | `stale_after_ms` | 30_000 (core `DEFAULT_STALE_AFTER_MS`) | heartbeat age past which a `live` heartbeat is stale |
//! | `crash_loop_warn_streak` | 3 | that many consecutive failed attempts or gate runs on one record is a warning |
//! | `crash_loop_critical_streak` | 5 | ... and that many, critical |
//!
//! ⚠ Spend is the ledger's lifetime total (the ledger has no billing window), so the cost
//! defaults are set well above a single day's work; tune them per workspace.

use fl_core::log::AttemptStatus;
use fl_core::telemetry::DEFAULT_STALE_AFTER_MS;
use serde::{Deserialize, Serialize};
use std::cmp::Reverse;
use std::collections::BTreeMap;

use crate::WatchError;
use crate::liveness::{WatchHealthReport, WatchLiveness};
use crate::rollups::WatchTelemetryReport;

/// How urgent an alert is. Orders `Info < Warning < Critical`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Info,
    Warning,
    Critical,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Info => "info",
            Severity::Warning => "warning",
            Severity::Critical => "critical",
        }
    }
}

/// The rule an alert comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WatchAlertRule {
    /// Recorded attempt spend at or above a cost threshold.
    Spend,
    /// A vendor quota window at or above a percentage threshold.
    Quota,
    /// An active agent whose heartbeat is stale (or whose host is unreachable).
    HeartbeatStale,
    /// An active agent whose heartbeat says live but whose process is gone.
    HeartbeatDead,
    /// Consecutive failed attempts or gate runs on one record.
    CrashLoop,
    /// A transition declared low regret whose gates cost far more than the project median
    /// (core `audit_transition_regret`). Advisory: always `info`.
    TransitionRegret,
}

impl WatchAlertRule {
    pub fn as_str(self) -> &'static str {
        match self {
            WatchAlertRule::Spend => "spend",
            WatchAlertRule::Quota => "quota",
            WatchAlertRule::HeartbeatStale => "heartbeat_stale",
            WatchAlertRule::HeartbeatDead => "heartbeat_dead",
            WatchAlertRule::CrashLoop => "crash_loop",
            WatchAlertRule::TransitionRegret => "transition_regret",
        }
    }
}

/// What kind of thing an alert is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WatchEntityKind {
    Workspace,
    Project,
    Agent,
    Vendor,
    Record,
    Transition,
}

impl WatchEntityKind {
    pub fn as_str(self) -> &'static str {
        match self {
            WatchEntityKind::Workspace => "workspace",
            WatchEntityKind::Project => "project",
            WatchEntityKind::Agent => "agent",
            WatchEntityKind::Vendor => "vendor",
            WatchEntityKind::Record => "record",
            WatchEntityKind::Transition => "transition",
        }
    }
}

/// The entity an alert is about.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchEntity {
    pub kind: WatchEntityKind,
    /// A stable id: a project or record IRI, an agent key, a vendor name, `workspace`.
    pub id: String,
    /// A readable name when the id is not one (a project slug).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// Which allowance window a quota reading is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuotaWindowKind {
    /// The short window (5 hours for the vendors that report one).
    Primary,
    /// The long window (7 days for the vendors that report one).
    Secondary,
}

impl QuotaWindowKind {
    pub fn as_str(self) -> &'static str {
        match self {
            QuotaWindowKind::Primary => "primary",
            QuotaWindowKind::Secondary => "secondary",
        }
    }
}

/// One vendor quota reading, as the caller (the hub's discovery) knows it. fl-watch has no
/// quota source of its own besides `watch-quota-*` runtime entries.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WatchQuotaWindow {
    /// The vendor (adapter) name: `claude`, `codex`, `muse`, ...
    pub vendor: String,
    pub window: QuotaWindowKind,
    /// The vendor's own percentage, 0 to 100.
    pub used_percent: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at_ms: Option<u64>,
    /// The session the reading came from, when there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_minutes: Option<u64>,
    /// The vendor's own words when it reported the allowance exhausted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exhausted_reason: Option<String>,
}

/// The limits alerts are measured against. Every field has a default (see the module docs),
/// so a partial JSON object is a valid set of thresholds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WatchThresholds {
    pub warn_quota_pct: f64,
    pub critical_quota_pct: f64,
    pub warn_cost_usd_micros: u64,
    pub critical_cost_usd_micros: u64,
    pub stale_after_ms: u64,
    pub crash_loop_warn_streak: u64,
    pub crash_loop_critical_streak: u64,
}

/// $100 in micro-USD.
pub const DEFAULT_WARN_COST_USD_MICROS: u64 = 100_000_000;
/// $500 in micro-USD.
pub const DEFAULT_CRITICAL_COST_USD_MICROS: u64 = 500_000_000;
pub const DEFAULT_WARN_QUOTA_PCT: f64 = 80.0;
pub const DEFAULT_CRITICAL_QUOTA_PCT: f64 = 95.0;
pub const DEFAULT_CRASH_LOOP_WARN_STREAK: u64 = 3;
pub const DEFAULT_CRASH_LOOP_CRITICAL_STREAK: u64 = 5;

impl Default for WatchThresholds {
    fn default() -> Self {
        Self {
            warn_quota_pct: DEFAULT_WARN_QUOTA_PCT,
            critical_quota_pct: DEFAULT_CRITICAL_QUOTA_PCT,
            warn_cost_usd_micros: DEFAULT_WARN_COST_USD_MICROS,
            critical_cost_usd_micros: DEFAULT_CRITICAL_COST_USD_MICROS,
            stale_after_ms: DEFAULT_STALE_AFTER_MS,
            crash_loop_warn_streak: DEFAULT_CRASH_LOOP_WARN_STREAK,
            crash_loop_critical_streak: DEFAULT_CRASH_LOOP_CRITICAL_STREAK,
        }
    }
}

impl WatchThresholds {
    /// Refuse thresholds that cannot be meant: a percentage that is not a number, negative or
    /// over 100; a warning above its critical level; a zero stale age or streak.
    pub fn validate(&self) -> Result<(), WatchError> {
        let bad = |m: String| Err(WatchError::InvalidThreshold(m));
        for (name, v) in [
            ("warn_quota_pct", self.warn_quota_pct),
            ("critical_quota_pct", self.critical_quota_pct),
        ] {
            if !v.is_finite() {
                return bad(format!("`{name}` must be a number, not {v}"));
            }
            if !(0.0..=100.0).contains(&v) {
                return bad(format!("`{name}` must be between 0 and 100, not {v}"));
            }
        }
        if self.warn_quota_pct > self.critical_quota_pct {
            return bad(format!(
                "`warn_quota_pct` ({}) is above `critical_quota_pct` ({})",
                self.warn_quota_pct, self.critical_quota_pct
            ));
        }
        if self.warn_cost_usd_micros > self.critical_cost_usd_micros {
            return bad(format!(
                "`warn_cost_usd_micros` ({}) is above `critical_cost_usd_micros` ({})",
                self.warn_cost_usd_micros, self.critical_cost_usd_micros
            ));
        }
        if self.stale_after_ms == 0 {
            return bad("`stale_after_ms` must be at least 1".into());
        }
        if self.crash_loop_warn_streak == 0 {
            return bad("`crash_loop_warn_streak` must be at least 1".into());
        }
        if self.crash_loop_warn_streak > self.crash_loop_critical_streak {
            return bad(format!(
                "`crash_loop_warn_streak` ({}) is above `crash_loop_critical_streak` ({})",
                self.crash_loop_warn_streak, self.crash_loop_critical_streak
            ));
        }
        Ok(())
    }
}

/// Dollars as the CLI takes them (`--warn-cost-usd 12.5`) to micro-USD. `None` for a value
/// that is not a number or is negative.
pub fn usd_to_micros(usd: f64) -> Option<u64> {
    if !usd.is_finite() || usd < 0.0 {
        return None;
    }
    let micros = (usd * 1_000_000.0).round();
    (micros <= u64::MAX as f64).then_some(micros as u64)
}

/// One active alert.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WatchAlert {
    /// `<rule>:<entity kind>:<entity id>` (quota: `quota:<vendor>:<window>`). Stable across polls.
    pub id: String,
    pub rule: WatchAlertRule,
    pub severity: Severity,
    pub entity: WatchEntity,
    pub message: String,
    /// The measured value (micro-USD, percent, streak length, heartbeat age in ms, ratio).
    pub observed: f64,
    /// The threshold the value met.
    pub threshold: f64,
    /// When the alert was evaluated (the `now_ms` passed in).
    pub at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at_ms: Option<u64>,
    pub remediation: String,
}

fn level<T: PartialOrd + Copy>(value: T, warn: T, critical: T) -> Option<(Severity, T)> {
    if value >= critical {
        Some((Severity::Critical, critical))
    } else if value >= warn {
        Some((Severity::Warning, warn))
    } else {
        None
    }
}

fn usd(micros: u64) -> String {
    format!(
        "${}.{:02}",
        micros / 1_000_000,
        (micros % 1_000_000) / 10_000
    )
}

/// Every alert `telemetry`, `health` and `quotas` raise against `thresholds` at `now_ms`,
/// most severe first, then by id. Pure and deterministic.
///
/// - **Spend**: each project in `telemetry.by_project`, and the workspace total when the
///   telemetry is not scoped to one project.
/// - **Quota**: per vendor and window, the highest finite reading. A reading whose
///   `resets_at_ms` is not after `now_ms` has already reset and raises nothing.
/// - **Heartbeat**: an active agent whose liveness is `stale` (warning) or `dead` (critical).
/// - **Crash loop**: the longer of a record's trailing failed-attempt and failed-gate streaks.
/// - **Transition regret**: each regret challenge, as `info`.
pub fn evaluate_watch_alerts(
    telemetry: &WatchTelemetryReport,
    health: &WatchHealthReport,
    quotas: &[WatchQuotaWindow],
    thresholds: &WatchThresholds,
    now_ms: u64,
) -> Vec<WatchAlert> {
    let mut out = Vec::new();
    let mut push = |rule: WatchAlertRule,
                    severity: Severity,
                    entity: WatchEntity,
                    id_tail: String,
                    message: String,
                    observed: f64,
                    threshold: f64,
                    resets_at_ms: Option<u64>,
                    remediation: String| {
        out.push(WatchAlert {
            id: format!("{}:{id_tail}", rule.as_str()),
            rule,
            severity,
            entity,
            message,
            observed,
            threshold,
            at_ms: now_ms,
            resets_at_ms,
            remediation,
        });
    };

    // Spend.
    let warn_cost = thresholds.warn_cost_usd_micros;
    let crit_cost = thresholds.critical_cost_usd_micros;
    if telemetry.project.is_none()
        && let Some((sev, limit)) = level(telemetry.totals.cost_usd_micros, warn_cost, crit_cost)
    {
        push(
            WatchAlertRule::Spend,
            sev,
            WatchEntity {
                kind: WatchEntityKind::Workspace,
                id: "workspace".into(),
                label: None,
            },
            "workspace".into(),
            format!(
                "workspace attempt spend {} is at or above the {} threshold {}",
                usd(telemetry.totals.cost_usd_micros),
                sev.as_str(),
                usd(limit)
            ),
            telemetry.totals.cost_usd_micros as f64,
            limit as f64,
            None,
            "review spend by project with `fl watch rollups`; lower attempt budgets or pause \
             the costliest agents"
                .into(),
        );
    }
    for p in &telemetry.by_project {
        if let Some((sev, limit)) = level(p.counts.cost_usd_micros, warn_cost, crit_cost) {
            push(
                WatchAlertRule::Spend,
                sev,
                WatchEntity {
                    kind: WatchEntityKind::Project,
                    id: p.project.clone(),
                    label: Some(p.slug.clone()),
                },
                format!("project:{}", p.project),
                format!(
                    "project `{}` attempt spend {} is at or above the {} threshold {}",
                    p.slug,
                    usd(p.counts.cost_usd_micros),
                    sev.as_str(),
                    usd(limit)
                ),
                p.counts.cost_usd_micros as f64,
                limit as f64,
                None,
                format!(
                    "review `fl stats --project {}`; lower attempt budgets or pause the \
                     project's agents",
                    p.project
                ),
            );
        }
    }

    // Quota: the highest finite, not-yet-reset reading per vendor and window.
    let mut worst: BTreeMap<(String, QuotaWindowKind), &WatchQuotaWindow> = BTreeMap::new();
    for q in quotas {
        if !q.used_percent.is_finite() || q.resets_at_ms.is_some_and(|r| r <= now_ms) {
            continue;
        }
        let key = (q.vendor.trim().to_ascii_lowercase(), q.window);
        match worst.get(&key) {
            Some(prev) if prev.used_percent >= q.used_percent => {}
            _ => {
                worst.insert(key, q);
            }
        }
    }
    for ((vendor, window), q) in worst {
        let Some((sev, limit)) = level(
            q.used_percent,
            thresholds.warn_quota_pct,
            thresholds.critical_quota_pct,
        ) else {
            continue;
        };
        let resets = match q.resets_at_ms {
            Some(r) => format!(
                ", resets in {} min",
                r.saturating_sub(now_ms).div_ceil(60_000)
            ),
            None => String::new(),
        };
        let reason = q
            .exhausted_reason
            .as_deref()
            .map(|r| format!(" ({r})"))
            .unwrap_or_default();
        push(
            WatchAlertRule::Quota,
            sev,
            WatchEntity {
                kind: WatchEntityKind::Vendor,
                id: vendor.clone(),
                label: q.session.clone(),
            },
            format!("{vendor}:{}", window.as_str()),
            format!(
                "{vendor} {} quota window is {:.1}% used{resets}{reason}",
                window.as_str(),
                q.used_percent
            ),
            q.used_percent,
            limit,
            q.resets_at_ms,
            format!(
                "remodel sessions on {vendor} onto an available vendor, or hold new work until \
                 the window resets"
            ),
        );
    }

    // Heartbeats.
    for a in &health.agents {
        if !a.active {
            continue;
        }
        let (rule, sev, what, hint) = match a.liveness {
            WatchLiveness::Stale => (
                WatchAlertRule::HeartbeatStale,
                Severity::Warning,
                "stale",
                format!(
                    "check the agent's host and process (`fl agent liveness {}`); restart the \
                     session if it hung",
                    a.agent
                ),
            ),
            WatchLiveness::Dead => (
                WatchAlertRule::HeartbeatDead,
                Severity::Critical,
                "dead: the heartbeat says live but the process is gone",
                "restart the hosted session or decommission it; its attempt was not recorded \
                 as finished"
                    .to_string(),
            ),
            _ => continue,
        };
        let age = a.heartbeat_age_ms.unwrap_or(0);
        push(
            rule,
            sev,
            WatchEntity {
                kind: WatchEntityKind::Agent,
                id: a.agent.clone(),
                label: a.node.clone(),
            },
            format!("agent:{}", a.agent),
            format!(
                "agent `{}` heartbeat is {what} (last beat {age} ms ago on {})",
                a.agent,
                a.node.as_deref().unwrap_or("an unknown node")
            ),
            age as f64,
            health.stale_after_ms as f64,
            None,
            hint,
        );
    }

    // Crash loops.
    for s in &telemetry.failure_streaks {
        let (streak, what) = if s.attempt_streak >= s.gate_streak {
            (s.attempt_streak, "failed attempts")
        } else {
            (s.gate_streak, "failed gate runs")
        };
        let Some((sev, limit)) = level(
            streak,
            thresholds.crash_loop_warn_streak,
            thresholds.crash_loop_critical_streak,
        ) else {
            continue;
        };
        let last = s
            .last_attempt_status
            .map(AttemptStatus::as_wire)
            .map(|w| format!(", last attempt {w}"))
            .unwrap_or_default();
        push(
            WatchAlertRule::CrashLoop,
            sev,
            WatchEntity {
                kind: WatchEntityKind::Record,
                id: s.record.clone(),
                label: Some(s.slug.clone()),
            },
            format!("record:{}", s.record),
            format!(
                "record {} in `{}` has {streak} consecutive {what}{last}",
                s.record, s.slug
            ),
            streak as f64,
            limit as f64,
            None,
            "stop retrying: read the record's last attempt and gate output, fix the cause, \
             then run it again"
                .into(),
        );
    }

    // Transition regret (advisory).
    for c in &telemetry.regret.challenges {
        let ch = &c.challenge;
        push(
            WatchAlertRule::TransitionRegret,
            Severity::Info,
            WatchEntity {
                kind: WatchEntityKind::Transition,
                id: format!("{}:{}", c.project, ch.name),
                label: Some(c.slug.clone()),
            },
            format!(
                "transition:{}:{}->{}:{}",
                c.project,
                ch.from.as_wire(),
                ch.to.as_wire(),
                ch.name
            ),
            format!(
                "transition `{}` ({} -> {}) in `{}` is declared low regret but its gates cost \
                 {}x the project median",
                ch.name,
                ch.from.as_wire(),
                ch.to.as_wire(),
                c.slug,
                ch.ratio
            ),
            ch.ratio as f64,
            fl_core::telemetry::REGRET_AUDIT_MULTIPLIER as f64,
            None,
            "declare the transition high regret, or make its gates cheaper".into(),
        );
    }

    out.sort_by(|a, b| {
        (Reverse(a.severity), a.id.as_str()).cmp(&(Reverse(b.severity), b.id.as_str()))
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::liveness::{WatchAgentHealth, WatchHealthReport, WatchLiveness};
    use crate::rollups::{
        WatchAttemptCounts, WatchFailureStreak, WatchProjectRollup, WatchRegretChallenge,
        WatchTelemetryReport,
    };
    use fl_core::model::State;
    use fl_core::telemetry::{LivenessVerdict, RegretChallenge, RegretMetric, UnverifiableReason};

    const NOW: u64 = 1_800_000_000_000;

    fn telemetry() -> WatchTelemetryReport {
        WatchTelemetryReport::default()
    }

    fn health() -> WatchHealthReport {
        WatchHealthReport {
            now_ms: NOW,
            stale_after_ms: DEFAULT_STALE_AFTER_MS,
            ..WatchHealthReport::default()
        }
    }

    fn eval(
        t: &WatchTelemetryReport,
        h: &WatchHealthReport,
        q: &[WatchQuotaWindow],
    ) -> Vec<WatchAlert> {
        evaluate_watch_alerts(t, h, q, &WatchThresholds::default(), NOW)
    }

    fn project(id: &str, cost: u64) -> WatchProjectRollup {
        WatchProjectRollup {
            project: id.into(),
            slug: id.trim_start_matches("urn:p:").into(),
            root: String::new(),
            counts: WatchAttemptCounts {
                cost_usd_micros: cost,
                ..WatchAttemptCounts::default()
            },
        }
    }

    fn quota(
        vendor: &str,
        window: QuotaWindowKind,
        pct: f64,
        resets: Option<u64>,
    ) -> WatchQuotaWindow {
        WatchQuotaWindow {
            vendor: vendor.into(),
            window,
            used_percent: pct,
            resets_at_ms: resets,
            session: Some(format!("{vendor}-session")),
            window_minutes: None,
            exhausted_reason: None,
        }
    }

    fn agent(name: &str, liveness: WatchLiveness, active: bool) -> WatchAgentHealth {
        WatchAgentHealth {
            agent: name.into(),
            liveness,
            active,
            verdict: LivenessVerdict::Unverifiable {
                reason: UnverifiableReason::StaleStatus,
                host: None,
                heartbeat_at: None,
            },
            node: Some("node-a".into()),
            project: None,
            adapter: None,
            attempt_id: None,
            record: None,
            heartbeat_at_ms: Some(NOW - 60_000),
            heartbeat_age_ms: Some(60_000),
            hosted_session: None,
        }
    }

    fn streak(record: &str, attempts: u64, gates: u64) -> WatchFailureStreak {
        WatchFailureStreak {
            record: record.into(),
            project: "urn:p:one".into(),
            slug: "one".into(),
            attempt_streak: attempts,
            gate_streak: gates,
            last_attempt_status: (attempts > 0).then_some(AttemptStatus::Crashed),
            last_gate_verdict: None,
        }
    }

    fn ids(alerts: &[WatchAlert]) -> Vec<(&str, Severity)> {
        alerts.iter().map(|a| (a.id.as_str(), a.severity)).collect()
    }

    #[test]
    fn defaults_are_documented_values_and_valid() {
        let t = WatchThresholds::default();
        assert_eq!(t.warn_quota_pct, 80.0);
        assert_eq!(t.critical_quota_pct, 95.0);
        assert_eq!(t.warn_cost_usd_micros, 100_000_000);
        assert_eq!(t.critical_cost_usd_micros, 500_000_000);
        assert_eq!(t.stale_after_ms, 30_000);
        assert_eq!(t.crash_loop_warn_streak, 3);
        assert_eq!(t.crash_loop_critical_streak, 5);
        t.validate().unwrap();
        // A partial object fills the rest from the defaults; an unknown key is refused.
        let partial: WatchThresholds =
            serde_json::from_value(serde_json::json!({"warn_quota_pct": 50.0})).unwrap();
        assert_eq!(partial.warn_quota_pct, 50.0);
        assert_eq!(partial.critical_quota_pct, 95.0);
        assert!(serde_json::from_value::<WatchThresholds>(serde_json::json!({"warn": 1})).is_err());
        let back: WatchThresholds =
            serde_json::from_str(&serde_json::to_string(&t).unwrap()).unwrap();
        assert_eq!(back, t);
    }

    #[test]
    fn validate_refuses_nan_negative_over_100_and_warn_above_critical() {
        let base = WatchThresholds::default();
        let cases: Vec<(&str, WatchThresholds)> = vec![
            (
                "NaN",
                WatchThresholds {
                    warn_quota_pct: f64::NAN,
                    ..base.clone()
                },
            ),
            (
                "inf",
                WatchThresholds {
                    critical_quota_pct: f64::INFINITY,
                    ..base.clone()
                },
            ),
            (
                "negative",
                WatchThresholds {
                    warn_quota_pct: -1.0,
                    ..base.clone()
                },
            ),
            (
                "over 100",
                WatchThresholds {
                    critical_quota_pct: 100.5,
                    ..base.clone()
                },
            ),
            (
                "pct order",
                WatchThresholds {
                    warn_quota_pct: 96.0,
                    critical_quota_pct: 95.0,
                    ..base.clone()
                },
            ),
            (
                "cost order",
                WatchThresholds {
                    warn_cost_usd_micros: 2,
                    critical_cost_usd_micros: 1,
                    ..base.clone()
                },
            ),
            (
                "stale zero",
                WatchThresholds {
                    stale_after_ms: 0,
                    ..base.clone()
                },
            ),
            (
                "streak zero",
                WatchThresholds {
                    crash_loop_warn_streak: 0,
                    ..base.clone()
                },
            ),
            (
                "streak order",
                WatchThresholds {
                    crash_loop_warn_streak: 6,
                    crash_loop_critical_streak: 5,
                    ..base.clone()
                },
            ),
        ];
        for (name, t) in cases {
            assert!(
                matches!(t.validate(), Err(WatchError::InvalidThreshold(_))),
                "{name} must be refused"
            );
        }
        // Edges that are allowed: 0 and 100, warn equal to critical.
        WatchThresholds {
            warn_quota_pct: 0.0,
            critical_quota_pct: 100.0,
            ..base.clone()
        }
        .validate()
        .unwrap();
        WatchThresholds {
            warn_quota_pct: 90.0,
            critical_quota_pct: 90.0,
            ..base.clone()
        }
        .validate()
        .unwrap();
        WatchThresholds {
            warn_cost_usd_micros: 7,
            critical_cost_usd_micros: 7,
            ..base
        }
        .validate()
        .unwrap();
    }

    #[test]
    fn usd_to_micros_rounds_and_refuses_nonsense() {
        assert_eq!(usd_to_micros(12.5), Some(12_500_000));
        assert_eq!(usd_to_micros(0.0), Some(0));
        assert_eq!(usd_to_micros(0.0000004), Some(0));
        assert_eq!(usd_to_micros(-0.01), None);
        assert_eq!(usd_to_micros(f64::NAN), None);
        assert_eq!(usd_to_micros(f64::INFINITY), None);
    }

    #[test]
    fn spend_below_at_and_above_each_threshold() {
        let warn = DEFAULT_WARN_COST_USD_MICROS;
        let crit = DEFAULT_CRITICAL_COST_USD_MICROS;
        for (cost, expect) in [
            (warn - 1, None),
            (warn, Some(Severity::Warning)),
            (warn + 1, Some(Severity::Warning)),
            (crit - 1, Some(Severity::Warning)),
            (crit, Some(Severity::Critical)),
            (crit + 1, Some(Severity::Critical)),
        ] {
            let mut t = telemetry();
            t.totals.cost_usd_micros = cost;
            t.by_project = vec![project("urn:p:one", cost)];
            let alerts = eval(&t, &health(), &[]);
            match expect {
                None => assert!(alerts.is_empty(), "{cost}: {alerts:?}"),
                Some(sev) => {
                    assert_eq!(
                        ids(&alerts),
                        vec![("spend:project:urn:p:one", sev), ("spend:workspace", sev)],
                        "{cost}"
                    );
                    let a = &alerts[0];
                    assert_eq!(a.entity.kind, WatchEntityKind::Project);
                    assert_eq!(a.entity.label.as_deref(), Some("one"));
                    assert_eq!(a.at_ms, NOW);
                    assert!(!a.remediation.is_empty());
                    assert_eq!(a.observed, cost as f64);
                }
            }
        }
    }

    #[test]
    fn a_project_scoped_report_raises_no_workspace_spend_alert() {
        let mut t = telemetry();
        t.project = Some("urn:p:one".into());
        t.totals.cost_usd_micros = DEFAULT_CRITICAL_COST_USD_MICROS;
        t.by_project = vec![project("urn:p:one", DEFAULT_CRITICAL_COST_USD_MICROS)];
        let alerts = eval(&t, &health(), &[]);
        assert_eq!(
            ids(&alerts),
            vec![("spend:project:urn:p:one", Severity::Critical)]
        );
    }

    #[test]
    fn quota_below_at_and_above_each_threshold_with_reset_and_hint() {
        let reset = NOW + 90 * 60_000;
        for (pct, expect) in [
            (79.9, None),
            (80.0, Some(Severity::Warning)),
            (80.1, Some(Severity::Warning)),
            (94.9, Some(Severity::Warning)),
            (95.0, Some(Severity::Critical)),
            (100.0, Some(Severity::Critical)),
        ] {
            let q = [quota("claude", QuotaWindowKind::Primary, pct, Some(reset))];
            let alerts = eval(&telemetry(), &health(), &q);
            match expect {
                None => assert!(alerts.is_empty(), "{pct}"),
                Some(sev) => {
                    assert_eq!(ids(&alerts), vec![("quota:claude:primary", sev)], "{pct}");
                    let a = &alerts[0];
                    assert_eq!(a.resets_at_ms, Some(reset));
                    assert!(a.message.contains("resets in 90 min"), "{}", a.message);
                    assert!(a.remediation.contains("remodel"), "{}", a.remediation);
                    assert_eq!(a.entity.kind, WatchEntityKind::Vendor);
                    assert_eq!(a.entity.label.as_deref(), Some("claude-session"));
                }
            }
        }
    }

    #[test]
    fn quota_keeps_the_worst_reading_per_vendor_window_and_skips_reset_or_nan() {
        let q = [
            quota("Claude", QuotaWindowKind::Primary, 81.0, None),
            quota("claude", QuotaWindowKind::Primary, 97.0, None),
            quota("claude", QuotaWindowKind::Secondary, 85.0, None),
            // Already reset at `now`: ignored.
            quota("codex", QuotaWindowKind::Primary, 99.0, Some(NOW)),
            // Not a number: ignored.
            quota("muse", QuotaWindowKind::Primary, f64::NAN, None),
            // Resets one millisecond from now: still counts.
            quota("grok", QuotaWindowKind::Secondary, 90.0, Some(NOW + 1)),
        ];
        let alerts = eval(&telemetry(), &health(), &q);
        assert_eq!(
            ids(&alerts),
            vec![
                ("quota:claude:primary", Severity::Critical),
                ("quota:claude:secondary", Severity::Warning),
                ("quota:grok:secondary", Severity::Warning),
            ]
        );
        assert_eq!(alerts[0].observed, 97.0);
    }

    #[test]
    fn heartbeat_alerts_only_for_active_stale_or_dead_agents() {
        let mut h = health();
        h.agents = vec![
            agent("ada", WatchLiveness::Stale, true),
            agent("bo", WatchLiveness::Dead, true),
            agent("cy", WatchLiveness::Live, true),
            agent("di", WatchLiveness::Exited, true),
            agent("ed", WatchLiveness::Missing, true),
            agent("fay", WatchLiveness::Dead, false),
            agent("gus", WatchLiveness::Stale, false),
        ];
        let alerts = eval(&telemetry(), &h, &[]);
        assert_eq!(
            ids(&alerts),
            vec![
                ("heartbeat_dead:agent:bo", Severity::Critical),
                ("heartbeat_stale:agent:ada", Severity::Warning),
            ]
        );
        assert!(alerts[1].message.contains("60000 ms ago on node-a"));
    }

    #[test]
    fn crash_loop_below_at_and_above_each_streak() {
        for (attempts, gates, expect) in [
            (2, 0, None),
            (3, 0, Some(Severity::Warning)),
            (0, 3, Some(Severity::Warning)),
            (4, 2, Some(Severity::Warning)),
            (5, 0, Some(Severity::Critical)),
            (1, 5, Some(Severity::Critical)),
            (9, 9, Some(Severity::Critical)),
        ] {
            let mut t = telemetry();
            t.failure_streaks = vec![streak("urn:r:1", attempts, gates)];
            let alerts = eval(&t, &health(), &[]);
            match expect {
                None => assert!(alerts.is_empty()),
                Some(sev) => {
                    assert_eq!(ids(&alerts), vec![("crash_loop:record:urn:r:1", sev)]);
                    let want = if attempts >= gates {
                        "failed attempts"
                    } else {
                        "failed gate runs"
                    };
                    assert!(alerts[0].message.contains(want), "{}", alerts[0].message);
                }
            }
        }
    }

    #[test]
    fn regret_challenges_are_info_alerts() {
        let mut t = telemetry();
        t.regret.challenges = vec![WatchRegretChallenge {
            project: "urn:p:one".into(),
            slug: "one".into(),
            challenge: RegretChallenge {
                name: "ship".into(),
                from: State::Review,
                to: State::Done,
                metric: RegretMetric::CostUsdMicros,
                measured_mean: 100_000,
                project_median: 1_000,
                ratio: 100,
            },
        }];
        let alerts = eval(&t, &health(), &[]);
        assert_eq!(
            ids(&alerts),
            vec![(
                "transition_regret:transition:urn:p:one:review->done:ship",
                Severity::Info
            )]
        );
    }

    #[test]
    fn order_is_severity_then_id_and_identical_input_gives_identical_output() {
        let mut t = telemetry();
        t.totals.cost_usd_micros = DEFAULT_WARN_COST_USD_MICROS;
        t.by_project = vec![
            project("urn:p:zz", DEFAULT_CRITICAL_COST_USD_MICROS),
            project("urn:p:aa", DEFAULT_WARN_COST_USD_MICROS),
        ];
        t.failure_streaks = vec![streak("urn:r:b", 5, 0), streak("urn:r:a", 3, 0)];
        let mut h = health();
        h.agents = vec![
            agent("zed", WatchLiveness::Stale, true),
            agent("amy", WatchLiveness::Dead, true),
        ];
        let q = [
            quota("muse", QuotaWindowKind::Primary, 99.0, None),
            quota("codex", QuotaWindowKind::Secondary, 85.0, None),
        ];
        let first = eval(&t, &h, &q);
        let severities: Vec<Severity> = first.iter().map(|a| a.severity).collect();
        let mut sorted = severities.clone();
        sorted.sort_by(|a, b| b.cmp(a));
        assert_eq!(severities, sorted, "most severe first");
        assert_eq!(
            ids(&first),
            vec![
                ("crash_loop:record:urn:r:b", Severity::Critical),
                ("heartbeat_dead:agent:amy", Severity::Critical),
                ("quota:muse:primary", Severity::Critical),
                ("spend:project:urn:p:zz", Severity::Critical),
                ("crash_loop:record:urn:r:a", Severity::Warning),
                ("heartbeat_stale:agent:zed", Severity::Warning),
                ("quota:codex:secondary", Severity::Warning),
                ("spend:project:urn:p:aa", Severity::Warning),
                ("spend:workspace", Severity::Warning),
            ]
        );
        // Same input, same output, byte for byte; input order does not matter.
        let again = eval(&t, &h, &q);
        assert_eq!(
            serde_json::to_string(&first).unwrap(),
            serde_json::to_string(&again).unwrap()
        );
        t.by_project.reverse();
        t.failure_streaks.reverse();
        h.agents.reverse();
        let mut q2 = q.to_vec();
        q2.reverse();
        let shuffled = eval(&t, &h, &q2);
        assert_eq!(
            serde_json::to_string(&first).unwrap(),
            serde_json::to_string(&shuffled).unwrap()
        );
    }

    #[test]
    fn custom_thresholds_move_the_lines() {
        let mut t = telemetry();
        t.by_project = vec![project("urn:p:one", 10)];
        t.project = Some("urn:p:one".into());
        let th = WatchThresholds {
            warn_cost_usd_micros: 5,
            critical_cost_usd_micros: 10,
            warn_quota_pct: 10.0,
            critical_quota_pct: 20.0,
            ..WatchThresholds::default()
        };
        let q = [quota("claude", QuotaWindowKind::Primary, 15.0, None)];
        let alerts = evaluate_watch_alerts(&t, &health(), &q, &th, NOW);
        assert_eq!(
            ids(&alerts),
            vec![
                ("spend:project:urn:p:one", Severity::Critical),
                ("quota:claude:primary", Severity::Warning),
            ]
        );
    }
}
