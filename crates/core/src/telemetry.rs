//! Adapter kinds, token rate cards, wrapper liveness heartbeats and evaluation,
//! and Decision 21 transition regret declaration auditing.

use crate::at::At;
use crate::ids::{ProjectId, RecordId};
use crate::iri::Iri;
use crate::log::{AttemptStatus, GateRun};
use crate::model::{Regret, State, Transition};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const DEFAULT_STALE_AFTER_MS: u64 = 30_000;
pub const REGRET_AUDIT_MULTIPLIER: u64 = 10;
pub const REGRET_AUDIT_MIN_COST_MICROS: u64 = 1_000;
pub const REGRET_AUDIT_MIN_DURATION_MS: u64 = 1_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdapterKind {
    Claude,
    Codex,
    Antigravity,
    Muse,
    Grok,
}

crate::wire::wire_names!(AdapterKind as adapter_kind_wire {
    Claude => "claude",
    Codex => "codex",
    Antigravity => "antigravity",
    Muse => "muse",
    Grok => "grok",
});
crate::wire::wire_parse!(AdapterKind as adapter_kind_parse);

impl AdapterKind {
    pub const fn default_binary(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Antigravity => "agy",
            Self::Muse => "muse",
            Self::Grok => "grok",
        }
    }
}

/// Optional per-adapter token pricing in micro-USD per 1,000,000 tokens.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RateCard {
    #[serde(default)]
    pub input_usd_micros_per_mtok: u64,
    #[serde(default)]
    pub output_usd_micros_per_mtok: u64,
}

impl RateCard {
    /// Compute cost in micro-USD when vendor-reported cost is absent or zero.
    /// Uses u128 intermediate math to prevent overflow.
    pub fn resolve_cost(
        self,
        tokens_in: u64,
        tokens_out: u64,
        vendor_cost_usd_micros: Option<u64>,
    ) -> u64 {
        if let Some(c) = vendor_cost_usd_micros
            && c > 0
        {
            return c;
        }
        if self.input_usd_micros_per_mtok == 0 && self.output_usd_micros_per_mtok == 0 {
            return 0;
        }
        let total = (tokens_in as u128)
            .saturating_mul(self.input_usd_micros_per_mtok as u128)
            .saturating_add(
                (tokens_out as u128).saturating_mul(self.output_usd_micros_per_mtok as u128),
            )
            / 1_000_000;
        u64::try_from(total).unwrap_or(u64::MAX)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LivenessStatus {
    Live,
    Unverifiable,
    Exited,
}

crate::wire::wire_names!(LivenessStatus as liveness_status_wire {
    Live => "live",
    Unverifiable => "unverifiable",
    Exited => "exited",
});
crate::wire::wire_parse!(LivenessStatus as liveness_status_parse);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnverifiableReason {
    MissingStatus,
    StaleStatus,
    HostUnavailable,
}

crate::wire::wire_names!(UnverifiableReason as unverifiable_reason_wire {
    MissingStatus => "missing_status",
    StaleStatus => "stale_status",
    HostUnavailable => "host_unavailable",
});
crate::wire::wire_parse!(UnverifiableReason as unverifiable_reason_parse);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttemptHeartbeat {
    pub attempt_id: Iri,
    pub project: ProjectId,
    pub record: RecordId,
    pub adapter: String,
    #[serde(default)]
    pub agent: Option<String>,
    pub host: String,
    pub pid: u32,
    pub started_at: At,
    pub heartbeat_at: At,
    pub status: LivenessStatus,
    #[serde(default)]
    pub outcome: Option<AttemptStatus>,
    #[serde(default)]
    pub exit_at: Option<At>,
    pub duration_ms: u64,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub cost_usd_micros: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum LivenessVerdict {
    Live {
        pid: u32,
        host: String,
        heartbeat_at: At,
        duration_ms: u64,
        tokens_in: u64,
        tokens_out: u64,
        cost_usd_micros: u64,
    },
    Unverifiable {
        reason: UnverifiableReason,
        #[serde(default)]
        host: Option<String>,
        #[serde(default)]
        heartbeat_at: Option<At>,
    },
    Exited {
        outcome: AttemptStatus,
        #[serde(default)]
        exit_at: Option<At>,
        duration_ms: u64,
        tokens_in: u64,
        tokens_out: u64,
        cost_usd_micros: u64,
    },
}

fn sanitize_slug_tail(raw: &str, max_len: usize) -> Option<String> {
    let mut out = String::new();
    let mut last_dash = false;
    for ch in raw.chars() {
        let lower = ch.to_ascii_lowercase();
        if lower.is_ascii_lowercase() || lower.is_ascii_digit() || lower == '_' {
            out.push(lower);
            last_dash = false;
        } else if !out.is_empty() && !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() || out.len() > max_len {
        None
    } else {
        Some(out)
    }
}

fn sha256_hex_36(input: &str) -> String {
    let digest = Sha256::digest(input.as_bytes());
    let mut hex = String::with_capacity(64);
    for b in digest {
        hex.push_str(&format!("{b:02x}"));
    }
    hex.truncate(36);
    hex
}

/// Deterministic `StateEntry.key` for a record's liveness entry, guaranteed to
/// pass `validate_slug` (`^[a-z][a-z0-9_-]{0,63}$`).
pub fn liveness_record_slug(record: &RecordId) -> String {
    let raw = record.iri().as_str();
    let stripped = raw.strip_prefix("urn:uuid:").unwrap_or(raw);
    match sanitize_slug_tail(stripped, 47) {
        Some(tail) => format!("liveness-record-{tail}"),
        None => format!("liveness-record-{}", sha256_hex_36(raw)),
    }
}

/// Deterministic `StateEntry.key` for an agent's liveness entry, guaranteed to
/// pass `validate_slug` (`^[a-z][a-z0-9_-]{0,63}$`).
pub fn liveness_agent_slug(persona: &str) -> String {
    match sanitize_slug_tail(persona.trim(), 48) {
        Some(tail) => format!("liveness-agent-{tail}"),
        None => format!("liveness-agent-{}", sha256_hex_36(persona.trim())),
    }
}

/// Evaluate an optional `AttemptHeartbeat` into the three-state liveness vocabulary.
pub fn evaluate_liveness<F>(
    hb: Option<&AttemptHeartbeat>,
    now: &At,
    stale_after_ms: u64,
    local_host: &str,
    pid_is_alive: F,
) -> LivenessVerdict
where
    F: Fn(u32) -> bool,
{
    let Some(hb) = hb else {
        return LivenessVerdict::Unverifiable {
            reason: UnverifiableReason::MissingStatus,
            host: None,
            heartbeat_at: None,
        };
    };
    match hb.status {
        LivenessStatus::Exited => LivenessVerdict::Exited {
            outcome: hb.outcome.unwrap_or(AttemptStatus::Crashed),
            exit_at: hb.exit_at.clone(),
            duration_ms: hb.duration_ms,
            tokens_in: hb.tokens_in,
            tokens_out: hb.tokens_out,
            cost_usd_micros: hb.cost_usd_micros,
        },
        LivenessStatus::Unverifiable => LivenessVerdict::Unverifiable {
            reason: UnverifiableReason::StaleStatus,
            host: Some(hb.host.clone()),
            heartbeat_at: Some(hb.heartbeat_at.clone()),
        },
        LivenessStatus::Live => {
            let age_ms = now
                .unix_millis()
                .saturating_sub(hb.heartbeat_at.unix_millis());
            if hb.host == local_host {
                if hb.pid == 0 || !pid_is_alive(hb.pid) {
                    return LivenessVerdict::Exited {
                        outcome: hb.outcome.unwrap_or(AttemptStatus::Crashed),
                        exit_at: None,
                        duration_ms: hb.duration_ms,
                        tokens_in: hb.tokens_in,
                        tokens_out: hb.tokens_out,
                        cost_usd_micros: hb.cost_usd_micros,
                    };
                }
                if age_ms <= stale_after_ms {
                    LivenessVerdict::Live {
                        pid: hb.pid,
                        host: hb.host.clone(),
                        heartbeat_at: hb.heartbeat_at.clone(),
                        duration_ms: hb.duration_ms,
                        tokens_in: hb.tokens_in,
                        tokens_out: hb.tokens_out,
                        cost_usd_micros: hb.cost_usd_micros,
                    }
                } else {
                    LivenessVerdict::Unverifiable {
                        reason: UnverifiableReason::StaleStatus,
                        host: Some(hb.host.clone()),
                        heartbeat_at: Some(hb.heartbeat_at.clone()),
                    }
                }
            } else if age_ms <= stale_after_ms {
                LivenessVerdict::Live {
                    pid: hb.pid,
                    host: hb.host.clone(),
                    heartbeat_at: hb.heartbeat_at.clone(),
                    duration_ms: hb.duration_ms,
                    tokens_in: hb.tokens_in,
                    tokens_out: hb.tokens_out,
                    cost_usd_micros: hb.cost_usd_micros,
                }
            } else {
                LivenessVerdict::Unverifiable {
                    reason: UnverifiableReason::HostUnavailable,
                    host: Some(hb.host.clone()),
                    heartbeat_at: Some(hb.heartbeat_at.clone()),
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RegretMetric {
    CostUsdMicros,
    DurationMs,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegretChallenge {
    pub name: String,
    pub from: State,
    pub to: State,
    pub metric: RegretMetric,
    pub measured_mean: u64,
    pub project_median: u64,
    pub ratio: u64,
}

/// Decision 21 advisory regret declaration audit.
pub fn audit_transition_regret(
    transitions: &[Transition],
    gate_runs: &[GateRun],
) -> Vec<RegretChallenge> {
    struct Measured<'a> {
        transition: &'a Transition,
        mean_cost: u64,
        mean_duration: u64,
    }

    let mut measured = Vec::new();
    for t in transitions {
        if t.gates.is_empty() {
            continue;
        }
        let mut count = 0u64;
        let mut sum_cost = 0u128;
        let mut sum_dur = 0u128;
        for run in gate_runs {
            if t.gates.contains(&run.gate) {
                count += 1;
                sum_cost += run.cost_usd_micros as u128;
                sum_dur += run.duration_ms as u128;
            }
        }
        if count == 0 {
            continue;
        }
        measured.push(Measured {
            transition: t,
            mean_cost: u64::try_from(sum_cost / count as u128).unwrap_or(u64::MAX),
            mean_duration: u64::try_from(sum_dur / count as u128).unwrap_or(u64::MAX),
        });
    }

    if measured.len() < 2 {
        return Vec::new();
    }

    let non_zero_cost_count = measured.iter().filter(|m| m.mean_cost > 0).count();
    let (metric, floor) = if non_zero_cost_count >= 2 {
        (RegretMetric::CostUsdMicros, REGRET_AUDIT_MIN_COST_MICROS)
    } else {
        (RegretMetric::DurationMs, REGRET_AUDIT_MIN_DURATION_MS)
    };

    let mut values: Vec<u64> = measured
        .iter()
        .map(|m| match metric {
            RegretMetric::CostUsdMicros => m.mean_cost,
            RegretMetric::DurationMs => m.mean_duration,
        })
        .collect();
    values.sort_unstable();
    let mid = values.len() / 2;
    let project_median = if values.len() % 2 == 1 {
        values[mid]
    } else {
        (values[mid - 1] + values[mid]) / 2
    };
    if project_median == 0 {
        return Vec::new();
    }

    let mut challenges = Vec::new();
    for m in &measured {
        if m.transition.regret != Regret::Low {
            continue;
        }
        let val = match metric {
            RegretMetric::CostUsdMicros => m.mean_cost,
            RegretMetric::DurationMs => m.mean_duration,
        };
        if val >= floor && val >= project_median.saturating_mul(REGRET_AUDIT_MULTIPLIER) {
            challenges.push(RegretChallenge {
                name: m.transition.name.clone(),
                from: m.transition.from,
                to: m.transition.to,
                metric,
                measured_mean: val,
                project_median,
                ratio: val / project_median,
            });
        }
    }
    challenges.sort_by(|a, b| {
        (a.from.as_wire(), a.to.as_wire(), a.name.as_str()).cmp(&(
            b.from.as_wire(),
            b.to.as_wire(),
            b.name.as_str(),
        ))
    });
    challenges
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{GateId, seq_iri};
    use crate::state::validate_slug;
    use crate::verdict::Verdict;

    #[test]
    fn adapter_kind_wire_names_and_default_binaries_round_trip() {
        let cases = [
            (AdapterKind::Claude, "claude", "claude"),
            (AdapterKind::Codex, "codex", "codex"),
            (AdapterKind::Antigravity, "antigravity", "agy"),
            (AdapterKind::Muse, "muse", "muse"),
            (AdapterKind::Grok, "grok", "grok"),
        ];
        for (kind, wire, binary) in cases {
            assert_eq!(kind.as_wire(), wire);
            assert_eq!(AdapterKind::from_wire(wire), Some(kind));
            assert_eq!(kind.default_binary(), binary);
            let json = serde_json::to_string(&kind).unwrap();
            assert_eq!(json, format!("\"{wire}\""));
            let back: AdapterKind = serde_json::from_str(&json).unwrap();
            assert_eq!(back, kind);
        }
        assert_eq!(AdapterKind::from_wire("unknown"), None);
    }

    #[test]
    fn liveness_status_and_unverifiable_reason_wire_names_round_trip() {
        for (status, wire) in [
            (LivenessStatus::Live, "live"),
            (LivenessStatus::Unverifiable, "unverifiable"),
            (LivenessStatus::Exited, "exited"),
        ] {
            assert_eq!(status.as_wire(), wire);
            assert_eq!(LivenessStatus::from_wire(wire), Some(status));
        }
        for (reason, wire) in [
            (UnverifiableReason::MissingStatus, "missing_status"),
            (UnverifiableReason::StaleStatus, "stale_status"),
            (UnverifiableReason::HostUnavailable, "host_unavailable"),
        ] {
            assert_eq!(reason.as_wire(), wire);
            assert_eq!(UnverifiableReason::from_wire(wire), Some(reason));
        }
    }

    #[test]
    fn rate_card_prefers_vendor_cost_and_falls_back_to_u128_token_pricing() {
        let card = RateCard {
            input_usd_micros_per_mtok: 3_000_000,   // $3.00 / MTok
            output_usd_micros_per_mtok: 15_000_000, // $15.00 / MTok
        };
        // Vendor cost > 0 takes precedence.
        assert_eq!(card.resolve_cost(10_000, 5_000, Some(42)), 42);
        // Vendor cost Some(0) or None falls back to rate card.
        // 10_000 * 3_000_000 / 1_000_000 + 5_000 * 15_000_000 / 1_000_000 = 30_000 + 75_000 = 105_000
        assert_eq!(card.resolve_cost(10_000, 5_000, Some(0)), 105_000);
        assert_eq!(card.resolve_cost(10_000, 5_000, None), 105_000);

        // Zero rate card produces 0.
        assert_eq!(RateCard::default().resolve_cost(100_000, 100_000, None), 0);

        // Large values use u128 intermediate math without overflowing u64 prematurely.
        let expensive = RateCard {
            input_usd_micros_per_mtok: 100_000_000,
            output_usd_micros_per_mtok: 100_000_000,
        };
        assert_eq!(
            expensive.resolve_cost(1_000_000_000, 1_000_000_000, None),
            200_000_000_000
        );
    }

    #[test]
    fn liveness_record_and_agent_slugs_always_pass_validate_slug() {
        let uuid_record = RecordId(seq_iri(42));
        let rec_slug = liveness_record_slug(&uuid_record);
        assert!(validate_slug(&rec_slug).is_ok(), "{rec_slug}");
        assert!(rec_slug.starts_with("liveness-record-"));

        // Non-UUID or overly long record IRI falls back to SHA-256 hex digest.
        let long_iri = Iri::parse(&format!("urn:fl:record:{}", "a".repeat(80))).unwrap();
        let long_slug = liveness_record_slug(&RecordId(long_iri));
        assert!(validate_slug(&long_slug).is_ok(), "{long_slug}");
        assert_eq!(long_slug.len(), "liveness-record-".len() + 36);

        for persona in [
            "ferris",
            "Jarvis",
            "agent_01",
            "  spaced Persona! ",
            "---",
            &"x".repeat(100),
        ] {
            let slug = liveness_agent_slug(persona);
            assert!(
                validate_slug(&slug).is_ok(),
                "invalid slug `{slug}` for `{persona}`"
            );
            assert!(slug.starts_with("liveness-agent-"));
        }
    }

    fn sample_heartbeat() -> AttemptHeartbeat {
        AttemptHeartbeat {
            attempt_id: seq_iri(1),
            project: ProjectId(seq_iri(2)),
            record: RecordId(seq_iri(3)),
            adapter: "claude".into(),
            agent: Some("ferris".into()),
            host: "ai-lab-0".into(),
            pid: 4242,
            started_at: At::from_unix_millis(10_000),
            heartbeat_at: At::from_unix_millis(20_000),
            status: LivenessStatus::Live,
            outcome: None,
            exit_at: None,
            duration_ms: 10_000,
            tokens_in: 120,
            tokens_out: 45,
            cost_usd_micros: 800,
        }
    }

    #[test]
    fn evaluate_liveness_covers_all_six_branches() {
        let now = At::from_unix_millis(25_000);
        let stale_now = At::from_unix_millis(60_000);

        // 1. Missing heartbeat -> Unverifiable(MissingStatus).
        assert_eq!(
            evaluate_liveness(None, &now, DEFAULT_STALE_AFTER_MS, "ai-lab-0", |_| true),
            LivenessVerdict::Unverifiable {
                reason: UnverifiableReason::MissingStatus,
                host: None,
                heartbeat_at: None,
            }
        );

        // 2. Exited status -> Exited.
        let mut exited_hb = sample_heartbeat();
        exited_hb.status = LivenessStatus::Exited;
        exited_hb.outcome = Some(AttemptStatus::Completed);
        exited_hb.exit_at = Some(At::from_unix_millis(22_000));
        assert_eq!(
            evaluate_liveness(
                Some(&exited_hb),
                &now,
                DEFAULT_STALE_AFTER_MS,
                "ai-lab-0",
                |_| true
            ),
            LivenessVerdict::Exited {
                outcome: AttemptStatus::Completed,
                exit_at: Some(At::from_unix_millis(22_000)),
                duration_ms: 10_000,
                tokens_in: 120,
                tokens_out: 45,
                cost_usd_micros: 800,
            }
        );

        // 3. Unverifiable status in heartbeat -> Unverifiable(StaleStatus).
        let mut unverifiable_hb = sample_heartbeat();
        unverifiable_hb.status = LivenessStatus::Unverifiable;
        assert_eq!(
            evaluate_liveness(
                Some(&unverifiable_hb),
                &now,
                DEFAULT_STALE_AFTER_MS,
                "ai-lab-0",
                |_| true
            ),
            LivenessVerdict::Unverifiable {
                reason: UnverifiableReason::StaleStatus,
                host: Some("ai-lab-0".into()),
                heartbeat_at: Some(At::from_unix_millis(20_000)),
            }
        );

        // 4. Local host: live PID + fresh heartbeat -> Live; dead PID -> Exited(Crashed); stale -> Unverifiable(StaleStatus).
        let live_hb = sample_heartbeat();
        assert_eq!(
            evaluate_liveness(
                Some(&live_hb),
                &now,
                DEFAULT_STALE_AFTER_MS,
                "ai-lab-0",
                |pid| pid == 4242
            ),
            LivenessVerdict::Live {
                pid: 4242,
                host: "ai-lab-0".into(),
                heartbeat_at: At::from_unix_millis(20_000),
                duration_ms: 10_000,
                tokens_in: 120,
                tokens_out: 45,
                cost_usd_micros: 800,
            }
        );
        assert_eq!(
            evaluate_liveness(
                Some(&live_hb),
                &now,
                DEFAULT_STALE_AFTER_MS,
                "ai-lab-0",
                |_| false
            ),
            LivenessVerdict::Exited {
                outcome: AttemptStatus::Crashed,
                exit_at: None,
                duration_ms: 10_000,
                tokens_in: 120,
                tokens_out: 45,
                cost_usd_micros: 800,
            }
        );
        assert_eq!(
            evaluate_liveness(
                Some(&live_hb),
                &stale_now,
                DEFAULT_STALE_AFTER_MS,
                "ai-lab-0",
                |_| true
            ),
            LivenessVerdict::Unverifiable {
                reason: UnverifiableReason::StaleStatus,
                host: Some("ai-lab-0".into()),
                heartbeat_at: Some(At::from_unix_millis(20_000)),
            }
        );

        // 5 & 6. Remote host: fresh -> Live; stale -> Unverifiable(HostUnavailable).
        assert_eq!(
            evaluate_liveness(
                Some(&live_hb),
                &now,
                DEFAULT_STALE_AFTER_MS,
                "other-host",
                |_| false
            ),
            LivenessVerdict::Live {
                pid: 4242,
                host: "ai-lab-0".into(),
                heartbeat_at: At::from_unix_millis(20_000),
                duration_ms: 10_000,
                tokens_in: 120,
                tokens_out: 45,
                cost_usd_micros: 800,
            }
        );
        assert_eq!(
            evaluate_liveness(
                Some(&live_hb),
                &stale_now,
                DEFAULT_STALE_AFTER_MS,
                "other-host",
                |_| true
            ),
            LivenessVerdict::Unverifiable {
                reason: UnverifiableReason::HostUnavailable,
                host: Some("ai-lab-0".into()),
                heartbeat_at: Some(At::from_unix_millis(20_000)),
            }
        );
    }

    fn make_run(gate: GateId, duration_ms: u64, cost_usd_micros: u64) -> GateRun {
        GateRun {
            id: Some(seq_iri(99)),
            at: Some(At::from_unix_millis(1_000)),
            gate,
            record: None,
            commit: "abc".into(),
            verdict: Verdict::from_predicate(true, 1),
            population: 1,
            output_excerpt: Some(String::new()),
            duration_ms,
            cost_usd_micros,
        }
    }

    #[test]
    fn audit_transition_regret_flags_low_regret_outliers_by_cost_and_duration() {
        let proj = ProjectId(seq_iri(1));
        let g_fast = GateId(seq_iri(10));
        let g_med = GateId(seq_iri(11));
        let g_heavy = GateId(seq_iri(12));

        let transitions = vec![
            Transition {
                project: proj.clone(),
                name: "start".into(),
                from: State::Todo,
                to: State::Doing,
                regret: Regret::Low,
                gates: vec![g_fast.clone()],
            },
            Transition {
                project: proj.clone(),
                name: "review".into(),
                from: State::Doing,
                to: State::Review,
                regret: Regret::Low,
                gates: vec![g_med.clone()],
            },
            Transition {
                project: proj.clone(),
                name: "ship".into(),
                from: State::Review,
                to: State::Done,
                regret: Regret::Low,
                gates: vec![g_heavy.clone()],
            },
        ];

        // Duration-based audit (when fewer than 2 transitions have non-zero cost):
        // means: 100ms, 200ms, 5_000ms -> median = 200ms; 5_000 >= 10 * 200 and >= 1_000ms floor.
        let dur_runs = vec![
            make_run(g_fast.clone(), 100, 0),
            make_run(g_med.clone(), 200, 0),
            make_run(g_heavy.clone(), 5_000, 0),
        ];
        let challenges = audit_transition_regret(&transitions, &dur_runs);
        assert_eq!(
            challenges,
            vec![RegretChallenge {
                name: "ship".into(),
                from: State::Review,
                to: State::Done,
                metric: RegretMetric::DurationMs,
                measured_mean: 5_000,
                project_median: 200,
                ratio: 25,
            }]
        );

        // Cost-based audit (when >= 2 transitions have non-zero cost):
        // means: 100, 300, 6_000 -> median = 300; 6_000 >= 10 * 300 and >= 1_000 micros floor.
        let cost_runs = vec![
            make_run(g_fast.clone(), 100, 100),
            make_run(g_med.clone(), 100, 300),
            make_run(g_heavy.clone(), 100, 6_000),
        ];
        let cost_challenges = audit_transition_regret(&transitions, &cost_runs);
        assert_eq!(
            cost_challenges,
            vec![RegretChallenge {
                name: "ship".into(),
                from: State::Review,
                to: State::Done,
                metric: RegretMetric::CostUsdMicros,
                measured_mean: 6_000,
                project_median: 300,
                ratio: 20,
            }]
        );

        // Declaring Regret::High silences the challenge.
        let mut high_transitions = transitions.clone();
        high_transitions[2].regret = Regret::High;
        assert!(audit_transition_regret(&high_transitions, &cost_runs).is_empty());

        // Below the noise floor (e.g. 500ms < 1_000ms floor even if >= 10x median of 20ms) is not challenged.
        let sub_floor_runs = vec![
            make_run(g_fast, 10, 0),
            make_run(g_med, 20, 0),
            make_run(g_heavy, 500, 0),
        ];
        assert!(audit_transition_regret(&transitions, &sub_floor_runs).is_empty());
    }
}
