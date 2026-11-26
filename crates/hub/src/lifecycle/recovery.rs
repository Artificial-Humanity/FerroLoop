//! Pure clock-driven recovery and notification policy. No store, runtime or wall clock access.
use super::*;

pub(super) const RECOVERY_INTERVAL_MS: u64 = 10 * 60 * 1_000;
pub(super) const RECOVERY_MAX_ATTEMPTS: u8 = 6;

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Decision {
    Wait,
    FastLaunch,
    SlowLaunch,
    Exhausted,
}

/// Classify a stopped record. Mutation must be saved before any probe or launch.
pub(super) fn classify(state: &mut HostedRespawnState, reason: &str, now: u64) -> Decision {
    classify_at_exit(state, reason, now, now)
}

/// A delayed exit can exhaust the fast budget as of its observation. Any existing
/// deadline or launch reservation still uses the current wall clock.
pub(super) fn classify_at_exit(
    state: &mut HostedRespawnState,
    reason: &str,
    now: u64,
    observed_at_ms: u64,
) -> Decision {
    state.recovery_healthy_since_ms = None;
    if state.recovery_started_at_ms.is_none() {
        let budget_at_ms = observed_at_ms.min(now);
        let guard_tripped = reason == STOP_REASON_CRASH_LOOP
            || state
                .respawn_timestamps_ms
                .iter()
                .filter(|&&ts| budget_at_ms.saturating_sub(ts) < AUTO_RESPAWN_WINDOW_MS)
                .count()
                >= AUTO_RESPAWN_MAX_IN_WINDOW;
        let prune_at_ms = if guard_tripped { budget_at_ms } else { now };
        state
            .respawn_timestamps_ms
            .retain(|ts| prune_at_ms.saturating_sub(*ts) < AUTO_RESPAWN_WINDOW_MS);
        state
            .exit_evidence
            .prune_fast_exits(&state.respawn_timestamps_ms, prune_at_ms);
        if guard_tripped {
            state.recovery_started_at_ms = Some(observed_at_ms);
            state.recovery_attempts = 0;
            state.next_allowed_at_ms = Some(observed_at_ms.saturating_add(RECOVERY_INTERVAL_MS));
            return Decision::Wait;
        }
    }
    if state.recovery_started_at_ms.is_some() {
        if state.recovery_attempts >= RECOVERY_MAX_ATTEMPTS {
            // No further launch deadline exists. Clearing it marks the final stopped
            // transition durably, so idle ticks do not repeat exhaustion events.
            state.next_allowed_at_ms = None;
            return Decision::Exhausted;
        }
        if respawn_due(state, now) {
            Decision::SlowLaunch
        } else {
            Decision::Wait
        }
    } else if respawn_due(state, now) {
        Decision::FastLaunch
    } else {
        Decision::Wait
    }
}

/// Reserve one launch durably after prerequisites pass. No elapsed time refills an incident.
pub(super) fn reserve_attempt(state: &mut HostedRespawnState, now: u64) {
    state.recovery_healthy_since_ms = None;
    if state.recovery_started_at_ms.is_some() {
        state.recovery_attempts = state.recovery_attempts.saturating_add(1);
        state.next_allowed_at_ms = Some(now.saturating_add(RECOVERY_INTERVAL_MS));
    } else {
        let idx = state
            .respawn_timestamps_ms
            .len()
            .min(AUTO_RESPAWN_BACKOFF_SECS.len() - 1);
        state.respawn_timestamps_ms.push(now);
        state.next_allowed_at_ms = Some(now.saturating_add(AUTO_RESPAWN_BACKOFF_SECS[idx] * 1_000));
    }
}

/// Before any process spawns, restore only this operation's reservation, then retain a bounded readiness cadence.
pub(super) fn refund_unspawned_attempt(
    state: &mut HostedRespawnState,
    before_reservation: &HostedRespawnState,
    now: u64,
) {
    *state = before_reservation.clone();
    defer_launch_prerequisite(state, now);
}

/// Unknown/gone observations break the healthy interval; a backwards clock starts it anew.
pub(super) fn observe_health(state: &mut HostedRespawnState, alive: bool, now: u64) {
    if state.recovery_started_at_ms.is_none() {
        return;
    }
    if !alive {
        state.recovery_healthy_since_ms = None;
        return;
    }
    match state.recovery_healthy_since_ms {
        Some(since) if now >= since && now - since >= RECOVERY_INTERVAL_MS => {
            state.clear_incident()
        }
        Some(since) if now >= since => {}
        _ => state.recovery_healthy_since_ms = Some(now),
    }
}

/// Input is an explicit registry projection; this function does no registry reads.
pub(super) fn notice_recipients(
    agents: &[(&str, &str, AgentRole, AgentStatus)],
    project: &str,
    persona: &str,
    stopped_lead: bool,
) -> Vec<String> {
    let leads: Vec<_> = agents
        .iter()
        .filter(|(_, p, role, status)| {
            *status == AgentStatus::Active
                && *role == AgentRole::Lead
                && p.eq_ignore_ascii_case(project)
        })
        .collect();
    let fallback = stopped_lead
        || leads.is_empty()
        || leads
            .iter()
            .any(|(name, _, _, _)| name.eq_ignore_ascii_case(persona));
    let mut names: BTreeSet<String> = leads
        .iter()
        .map(|(name, _, _, _)| name.to_ascii_lowercase())
        .collect();
    if fallback {
        names.extend(
            agents
                .iter()
                .filter(|(_, _, role, status)| {
                    *status == AgentStatus::Active && *role == AgentRole::LabManager
                })
                .map(|(name, _, _, _)| name.to_ascii_lowercase()),
        );
    }
    names.retain(|name| !name.eq_ignore_ascii_case(persona));
    names.into_iter().collect()
}

/// Phase prefixes let the same recipient receive both incident and final-stop notices.
pub(super) fn notice_key(state: &HostedRespawnState, recipient: &str) -> String {
    format!(
        "{}:{}",
        if state.recovery_attempts >= RECOVERY_MAX_ATTEMPTS {
            "exhausted"
        } else {
            "recovery"
        },
        recipient.to_ascii_lowercase()
    )
}
pub(super) fn notice_delivered(state: &HostedRespawnState, recipient: &str) -> bool {
    state
        .crash_loop_notified_recipients
        .contains(&notice_key(state, recipient))
}

pub(super) fn deliver_pending(
    state: &mut HostedRespawnState,
    recipients: &[String],
    mut send: impl FnMut(&str) -> bool,
) {
    for recipient in recipients {
        if !notice_delivered(state, recipient) && send(recipient) {
            state
                .crash_loop_notified_recipients
                .push(notice_key(state, recipient));
        }
    }
    state.crash_loop_notified =
        !recipients.is_empty() && recipients.iter().all(|r| notice_delivered(state, r));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovery_preserves_fast_guard_and_initializes_legacy_records() {
        let mut s = HostedRespawnState::default();
        for now in [1_000, 31_000, 91_000] {
            assert_eq!(
                classify(&mut s, STOP_REASON_RUNTIME_GONE, now),
                Decision::FastLaunch
            );
            reserve_attempt(&mut s, now);
        }
        assert_eq!(
            classify(&mut s, STOP_REASON_RUNTIME_GONE, 100_000),
            Decision::Wait
        );
        assert_eq!(s.recovery_started_at_ms, Some(100_000));
        assert_eq!(s.next_allowed_at_ms, Some(700_000));
        let mut old: HostedRespawnState =
            serde_json::from_str(r#"{"crash_loop_notified":true}"#).unwrap();
        assert_eq!(
            classify(&mut old, STOP_REASON_CRASH_LOOP, 5_000),
            Decision::Wait
        );
        assert_eq!(old.next_allowed_at_ms, Some(605_000));
        assert_eq!(old.recovery_attempts, 0);
    }

    #[test]
    fn recovery_six_attempt_ceiling_survives_early_deaths_and_clock_jumps() {
        let mut s = HostedRespawnState::default();
        classify(&mut s, STOP_REASON_CRASH_LOOP, 1_000);
        for n in 0..6 {
            let due = s.next_allowed_at_ms.unwrap();
            assert_eq!(
                classify(&mut s, STOP_REASON_RUNTIME_GONE, due - 1),
                Decision::Wait
            );
            assert_eq!(
                classify(&mut s, STOP_REASON_RUNTIME_GONE, due),
                Decision::SlowLaunch
            );
            reserve_attempt(&mut s, due);
            assert_eq!(s.recovery_attempts, n + 1);
            assert_eq!(s.next_allowed_at_ms, Some(due + RECOVERY_INTERVAL_MS));
        }
        assert_eq!(
            classify(&mut s, STOP_REASON_RUNTIME_GONE, u64::MAX),
            Decision::Exhausted
        );
        let mut overdue = HostedRespawnState::default();
        classify(&mut overdue, STOP_REASON_CRASH_LOOP, 1_000);
        assert_eq!(
            classify(&mut overdue, STOP_REASON_CRASH_LOOP, 9_000_000),
            Decision::SlowLaunch
        );
        reserve_attempt(&mut overdue, 9_000_000);
        assert_eq!(
            classify(&mut overdue, STOP_REASON_RUNTIME_GONE, 9_000_000),
            Decision::Wait
        );
        assert_eq!(
            classify(&mut overdue, STOP_REASON_RUNTIME_GONE, 1),
            Decision::Wait
        );
        assert_eq!(overdue.recovery_attempts, 1);
    }

    #[test]
    fn exhaustion_classification_is_stable_after_final_stop() {
        let mut state = HostedRespawnState {
            recovery_started_at_ms: Some(1),
            recovery_attempts: 6,
            next_allowed_at_ms: Some(601_000),
            ..Default::default()
        };
        assert_eq!(
            classify(&mut state, STOP_REASON_CRASH_LOOP, 2_000),
            Decision::Exhausted
        );
        assert_eq!(state.next_allowed_at_ms, None);
        let stopped = state.clone();
        for now in [17_000, 9_000_000] {
            assert_eq!(
                classify(&mut state, STOP_REASON_CRASH_LOOP, now),
                Decision::Exhausted
            );
            assert_eq!(state, stopped);
        }
    }

    #[test]
    fn recovery_prerequisite_deferral_is_uncharged_and_never_shortens_slow_deadline() {
        let mut s = HostedRespawnState::default();
        classify(&mut s, STOP_REASON_CRASH_LOOP, 1_000);
        defer_launch_prerequisite(&mut s, 2_000);
        assert_eq!(s.next_allowed_at_ms, Some(601_000));
        assert_eq!(s.recovery_attempts, 0);
        assert!(s.respawn_timestamps_ms.is_empty());
        defer_launch_prerequisite(&mut s, 601_000);
        assert_eq!(s.next_allowed_at_ms, Some(661_000));
        assert_eq!(
            classify(&mut s, STOP_REASON_PREREQUISITE_UNAVAILABLE, 601_000),
            Decision::Wait
        );
    }

    #[test]
    fn recovery_requires_observed_healthy_interval_and_preserves_unknown_gaps() {
        let mut s = HostedRespawnState::default();
        classify(&mut s, STOP_REASON_CRASH_LOOP, 1_000);
        reserve_attempt(&mut s, 601_000);
        observe_health(&mut s, true, 602_000);
        observe_health(&mut s, true, 1_201_999);
        assert_eq!(s.recovery_attempts, 1);
        observe_health(&mut s, false, 1_202_000);
        assert_eq!(s.recovery_healthy_since_ms, None);
        observe_health(&mut s, true, 1_203_000);
        observe_health(&mut s, true, 1_803_000);
        assert_eq!(s, HostedRespawnState::default());
        classify(&mut s, STOP_REASON_CRASH_LOOP, 9_000);
        observe_health(&mut s, true, 10_000);
        observe_health(&mut s, true, 1);
        assert_eq!(s.recovery_healthy_since_ms, Some(1));
        assert!(s.recovery_started_at_ms.is_some());
    }

    #[test]
    fn recovery_fields_are_backward_compatible_and_nonempty_state_is_retained() {
        let empty: HostedRespawnState = serde_json::from_str("{}").unwrap();
        assert!(empty.is_empty());
        assert_eq!(serde_json::to_string(&empty).unwrap(), "{}");
        let mut s = empty;
        s.recovery_started_at_ms = Some(1);
        s.recovery_attempts = 2;
        s.recovery_healthy_since_ms = Some(3);
        s.crash_loop_notified_recipients = vec!["recovery:lead".into()];
        assert!(!s.is_empty());
        assert_eq!(
            serde_json::from_value::<HostedRespawnState>(serde_json::to_value(&s).unwrap())
                .unwrap(),
            s
        );
    }

    #[test]
    fn recovery_notice_policy_falls_back_by_role_and_tracks_each_phase() {
        let roles = vec![
            ("lead", "project", AgentRole::Lead, AgentStatus::Active),
            (
                "manager",
                "workspace",
                AgentRole::LabManager,
                AgentStatus::Active,
            ),
            ("other", "elsewhere", AgentRole::Lead, AgentStatus::Active),
            (
                "retired",
                "workspace",
                AgentRole::LabManager,
                AgentStatus::Retired,
            ),
        ];
        assert_eq!(
            notice_recipients(&roles, "project", "worker", false),
            vec!["lead"]
        );
        assert_eq!(
            notice_recipients(&roles, "project", "lead", true),
            vec!["manager"]
        );
        assert_eq!(
            notice_recipients(&roles, "missing", "worker", false),
            vec!["manager"]
        );
        assert_eq!(
            notice_recipients(&roles, "missing", "manager", false),
            Vec::<String>::new()
        );
        let mut s = HostedRespawnState {
            crash_loop_notified: true,
            ..Default::default()
        };
        assert!(!notice_delivered(&s, "manager"));
        s.crash_loop_notified_recipients
            .push(notice_key(&s, "manager"));
        assert!(notice_delivered(&s, "manager"));
        assert!(!notice_delivered(&s, "new-lead"));
        s.recovery_attempts = RECOVERY_MAX_ATTEMPTS;
        assert!(!notice_delivered(&s, "manager"));
    }
    #[test]
    fn recovery_notice_delivery_retries_only_failed_and_new_recipients() {
        let mut s = HostedRespawnState::default();
        let recipients = vec!["manager-a".into(), "manager-b".into()];
        let mut calls = Vec::new();
        deliver_pending(&mut s, &recipients, |name| {
            calls.push(name.to_string());
            name == "manager-b"
        });
        assert_eq!(calls, recipients);
        assert!(!s.crash_loop_notified);
        calls.clear();
        deliver_pending(&mut s, &recipients, |name| {
            calls.push(name.to_string());
            true
        });
        assert_eq!(calls, vec!["manager-a"]);
        assert!(s.crash_loop_notified);
        calls.clear();
        let expanded = vec!["manager-a".into(), "manager-b".into(), "new-lead".into()];
        deliver_pending(&mut s, &expanded, |name| {
            calls.push(name.to_string());
            true
        });
        assert_eq!(calls, vec!["new-lead"]);
        s.recovery_attempts = RECOVERY_MAX_ATTEMPTS;
        calls.clear();
        deliver_pending(&mut s, &expanded, |name| {
            calls.push(name.to_string());
            true
        });
        assert_eq!(calls, expanded);
    }
    #[test]
    fn automatic_preparation_errors_reserve_slow_budget_and_spacing() {
        let mut state = HostedRespawnState {
            recovery_started_at_ms: Some(1_000),
            recovery_attempts: 2,
            next_allowed_at_ms: Some(601_000),
            ..Default::default()
        };
        reserve_automatic_respawn(&mut state, 601_000).unwrap();
        assert_eq!(state.recovery_attempts, 3);
        assert!(state.respawn_timestamps_ms.is_empty());
        assert_eq!(state.next_allowed_at_ms, Some(1_201_000));
        assert_eq!(
            classify(&mut state, STOP_REASON_CRASH_LOOP, 601_001),
            Decision::Wait
        );
        state.recovery_attempts = RECOVERY_MAX_ATTEMPTS;
        assert!(matches!(
            reserve_automatic_respawn(&mut state, 1_201_000),
            Err(LifecycleError::Conflict(_))
        ));
        assert_eq!(state.recovery_attempts, RECOVERY_MAX_ATTEMPTS);
    }

    #[test]
    fn unspawned_prerequisite_refusal_restores_slow_incident_and_readiness_deadline() {
        let before = HostedRespawnState {
            respawn_timestamps_ms: vec![900],
            recovery_started_at_ms: Some(1_000),
            recovery_attempts: 2,
            next_allowed_at_ms: Some(701_000),
            crash_loop_notified_recipients: vec!["recovery:lead".into()],
            ..Default::default()
        };
        let mut state = before.clone();
        reserve_attempt(&mut state, 601_000);
        assert_eq!(state.recovery_attempts, 3);
        refund_unspawned_attempt(&mut state, &before, 601_000);
        assert_eq!(
            state, before,
            "an existing later deadline must survive the refund"
        );
        let mut due = before.clone();
        due.next_allowed_at_ms = Some(601_000);
        let mut attempted = due.clone();
        reserve_attempt(&mut attempted, 601_000);
        refund_unspawned_attempt(&mut attempted, &due, 601_000);
        due.next_allowed_at_ms = Some(661_000);
        assert_eq!(attempted, due);
    }
}
