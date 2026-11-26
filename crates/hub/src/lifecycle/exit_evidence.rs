//! Hub-private evidence for the child exits counted by hosted recovery.
use super::*;
use fl_host_proto::{HostedExitEvidence, HostedExitEvidenceStatus, StreamId};

const MAX_EXITS: usize = 1 + AUTO_RESPAWN_MAX_IN_WINDOW + recovery::RECOVERY_MAX_ATTEMPTS as usize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum LaunchAttempt {
    Baseline,
    Fast(u64),
    Slow(u8),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct HostedLaunch {
    stream: StreamId,
    generation: String,
    attempt: LaunchAttempt,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct CountedExit {
    stream: StreamId,
    attempt: LaunchAttempt,
    observed_at_ms: u64,
    evidence: Option<HostedExitEvidence>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostedExitIncident {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    launch: Option<HostedLaunch>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    exits: Vec<CountedExit>,
}

impl HostedExitIncident {
    pub(super) fn is_empty(&self) -> bool {
        self.launch.is_none() && self.exits.is_empty()
    }

    pub(super) fn latest_evidence(&self) -> Option<&HostedExitEvidence> {
        self.exits
            .iter()
            .rev()
            .find_map(|exit| exit.evidence.as_ref())
    }

    pub(super) fn clear_incident(&mut self) {
        self.exits.clear();
        if let Some(launch) = &mut self.launch {
            launch.attempt = LaunchAttempt::Baseline;
        }
    }

    pub(super) fn clear_launch(&mut self) -> bool {
        self.launch.take().is_some()
    }

    pub(super) fn keep_live_launch(
        &mut self,
        runtime: &dyn SessionRuntime,
        record: &HostedSessionRecord,
    ) {
        if !self
            .matching_launch(record)
            .is_some_and(|launch| runtime.owns_host_stream(&launch.stream, &record.session_id))
        {
            self.clear_launch();
        }
    }

    fn matching_launch(&self, record: &HostedSessionRecord) -> Option<&HostedLaunch> {
        self.launch.as_ref().filter(|launch| {
            record.hydration_generation.as_deref() == Some(launch.generation.as_str())
        })
    }

    pub(super) fn prune_fast_exits(&mut self, fast_attempts: &[u64], now: u64) {
        self.exits.retain(|entry| match entry.attempt {
            LaunchAttempt::Baseline => {
                now.saturating_sub(entry.observed_at_ms) < AUTO_RESPAWN_WINDOW_MS
            }
            LaunchAttempt::Fast(at) => fast_attempts.contains(&at),
            LaunchAttempt::Slow(_) => false,
        });
    }

    /// An exact stream works even when the child never survived long enough to record a PID.
    /// A stopped-record write can fail after the evidence write, so reuse the original observation.
    pub(super) fn capture(
        &self,
        runtime: &dyn SessionRuntime,
        record: &HostedSessionRecord,
    ) -> Option<HostedExitEvidence> {
        let launch = self.matching_launch(record)?;
        if let Some(counted) = self
            .exits
            .iter()
            .find(|entry| entry.stream == launch.stream)
        {
            return counted.evidence.clone();
        }
        match runtime.hosted_exit_evidence(&launch.stream, &record.session_id) {
            HostedExitEvidenceStatus::Exited(evidence)
                if evidence.stream == launch.stream
                    && evidence.terminal_tail.len()
                        <= fl_host_proto::MAX_HOSTED_EXIT_TAIL_BYTES
                    && evidence.terminal_tail.lines().count()
                        <= fl_host_proto::MAX_HOSTED_EXIT_TAIL_LINES
                    && evidence
                        .terminal_tail
                        .chars()
                        .all(|ch| !ch.is_control() || matches!(ch, '\n' | '\t')) =>
            {
                Some(evidence)
            }
            _ => None,
        }
    }

    pub(super) fn count_exit(
        &mut self,
        record: &HostedSessionRecord,
        evidence: Option<HostedExitEvidence>,
        now: u64,
    ) -> bool {
        let Some(launch) = self.matching_launch(record) else {
            return false;
        };
        if self.exits.iter().any(|entry| entry.stream == launch.stream) {
            return false;
        }
        let entry = CountedExit {
            stream: launch.stream.clone(),
            attempt: launch.attempt,
            observed_at_ms: now,
            evidence,
        };
        if self.exits.len() >= MAX_EXITS {
            self.exits.drain(..self.exits.len() - MAX_EXITS + 1);
        }
        self.exits.push(entry);
        true
    }
}

pub(super) fn spawn_and_remember<S: StateStore>(
    store: &S,
    env: &LifecycleEnv<'_>,
    state: &mut HostedRespawnState,
    record: &HostedSessionRecord,
    launch: Option<&LaunchSpec>,
    by: &str,
) -> Result<bool, LifecycleError> {
    let (session, window) = runtime_names(&record.vendor, &record.persona);
    let (spawned, stream) = env.runtime.spawn_with_stream(
        &session,
        &window,
        record.cwd.as_deref(),
        launch,
        &record.session_id,
    );
    if spawned {
        remember_spawn(store, env, state, record, stream, by)?;
    }
    Ok(spawned)
}

pub(super) fn save_counted_exit<S: StateStore>(
    store: &S,
    record: &HostedSessionRecord,
    state: &mut HostedRespawnState,
    evidence: Option<HostedExitEvidence>,
    by: &str,
) -> Result<(), LifecycleError> {
    if state
        .exit_evidence
        .count_exit(record, evidence, current_respawn_clock_ms())
    {
        save_hosted_respawn_state(store, &record.session_id, state, by)?;
        if let Some(exit) = state.exit_evidence.exits.last() {
            let detail = exit.evidence.as_ref().map_or_else(
                || "evidence: unavailable".to_string(),
                |evidence| exit_status(evidence),
            );
            eprintln!(
                "hub: warning ({}): counted hosted child exit at epoch {}. {detail}",
                record.session_id, exit.stream.epoch.0,
            );
        }
    }
    Ok(())
}

fn remember_spawn<S: StateStore>(
    store: &S,
    env: &LifecycleEnv<'_>,
    state: &mut HostedRespawnState,
    record: &HostedSessionRecord,
    stream: Option<StreamId>,
    by: &str,
) -> Result<(), LifecycleError> {
    let (Some(stream), Some(generation)) = (stream, record.hydration_generation.as_ref()) else {
        return Ok(());
    };
    let attempt = if by != AUTO_RESPAWN_ACTOR {
        LaunchAttempt::Baseline
    } else if state.recovery_started_at_ms.is_some() {
        LaunchAttempt::Slow(state.recovery_attempts)
    } else {
        state
            .respawn_timestamps_ms
            .last()
            .map_or(LaunchAttempt::Baseline, |at| LaunchAttempt::Fast(*at))
    };
    state.exit_evidence.launch = Some(HostedLaunch {
        stream: stream.clone(),
        generation: generation.clone(),
        attempt,
    });
    if let Err(err) = save_hosted_respawn_state(store, &record.session_id, state, by) {
        // The initial running record already exists. Do not leave a new child alive
        // without the durable stream binding its future recovery will need.
        let target = hosted_tmux_target(&record.vendor, &record.persona);
        env.runtime
            .rollback_host_spawn(&stream, env.runtime.unit(&target), &record.session_id);
        return Err(err);
    }
    Ok(())
}

fn complete_failure(evidence: &HostedExitEvidence) -> bool {
    match (evidence.exit.code, evidence.exit.signal) {
        (Some(code), None) => code != 0,
        (None, Some(signal)) => signal > 0,
        _ => false,
    }
}

fn deterministic(state: &HostedRespawnState, first: &HostedExitEvidence) -> bool {
    let exits = &state.exit_evidence.exits;
    let fast = &state.respawn_timestamps_ms;
    let slow = state.recovery_attempts;
    if exits.len() < 2
        || exits.len() != 1 + fast.len() + usize::from(slow)
        || !complete_failure(first)
    {
        return false;
    }
    let exactly_one = |attempt| {
        exits
            .iter()
            .filter(|entry| entry.attempt == attempt)
            .count()
            == 1
    };
    exactly_one(LaunchAttempt::Baseline)
        && fast.iter().all(|at| exactly_one(LaunchAttempt::Fast(*at)))
        && (1..=slow).all(|n| exactly_one(LaunchAttempt::Slow(n)))
        && exits.iter().all(|entry| {
            entry.evidence.as_ref().is_some_and(|evidence| {
                evidence.exit == first.exit && evidence.terminal_tail == first.terminal_tail
            })
        })
}

fn exit_status(evidence: &HostedExitEvidence) -> String {
    match (evidence.exit.code, evidence.exit.signal) {
        (Some(code), None) => format!("exit code {code}"),
        (None, Some(signal)) => format!("signal {signal}"),
        _ => "unknown exit status".into(),
    }
}

/// Safe to use in journal messages, durable Wire bodies and other API-visible summaries.
pub(super) fn summary(state: &HostedRespawnState) -> String {
    let Some(evidence) = state.exit_evidence.latest_evidence() else {
        return "evidence: unavailable".into();
    };
    let assessment = if deterministic(state, evidence) {
        "deterministic launch failure"
    } else {
        "mixed or incomplete launch evidence"
    };
    format!(
        "evidence: {assessment}. Latest available {}, launched {} UTC, captured {} UTC",
        exit_status(evidence),
        fl_core::At::from_unix_millis(evidence.started_at_ms),
        fl_core::At::from_unix_millis(evidence.captured_at_ms),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use fl_host_proto::{ChildExit, HostInstance, SessionEpoch, SessionId};

    fn evidence(
        epoch: u64,
        code: Option<i32>,
        signal: Option<i32>,
        tail: &str,
    ) -> HostedExitEvidence {
        HostedExitEvidence {
            stream: StreamId {
                host: HostInstance::new([2; 16]),
                session: SessionId::new("fixture.claude").unwrap(),
                epoch: SessionEpoch(epoch),
            },
            exit: ChildExit { code, signal },
            started_at_ms: epoch,
            captured_at_ms: epoch + 1,
            terminal_tail: tail.into(),
        }
    }

    #[test]
    fn hosted_exit_evidence_requires_all_counted_failures_to_match() {
        let fixed_tail = "private deterministic failure";
        let baseline = evidence(1, Some(1), None, fixed_tail);
        let fast = evidence(2, Some(1), None, fixed_tail);
        let mut state = HostedRespawnState {
            respawn_timestamps_ms: vec![2],
            exit_evidence: HostedExitIncident {
                launch: None,
                exits: vec![
                    CountedExit {
                        stream: baseline.stream.clone(),
                        attempt: LaunchAttempt::Baseline,
                        observed_at_ms: 2,
                        evidence: Some(baseline),
                    },
                    CountedExit {
                        stream: fast.stream.clone(),
                        attempt: LaunchAttempt::Fast(2),
                        observed_at_ms: 3,
                        evidence: Some(fast),
                    },
                ],
            },
            ..Default::default()
        };
        assert!(summary(&state).contains("deterministic launch failure"));
        assert!(!summary(&state).contains(fixed_tail));
        for second in [
            Some(evidence(2, Some(1), None, "another terminal line")),
            Some(evidence(2, None, Some(1), fixed_tail)),
            None,
        ] {
            state.exit_evidence.exits[1].evidence = second;
            assert!(summary(&state).contains("mixed or incomplete launch evidence"));
        }
        state.exit_evidence.exits[0].evidence = None;
        assert_eq!(summary(&state), "evidence: unavailable");
    }
}
