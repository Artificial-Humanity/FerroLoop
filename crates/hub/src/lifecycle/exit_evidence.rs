//! Hub-private evidence for the child exits counted by hosted recovery.
use super::*;
use fl_host_proto::{HostedExitEvidence, HostedExitEvidenceStatus, StreamId};

const MAX_EXITS: usize = 1 + AUTO_RESPAWN_MAX_IN_WINDOW + recovery::RECOVERY_MAX_ATTEMPTS as usize;
// Allow the Host's post-leader-exit cleanup and terminal drain to finish. A normal
// monitor tick can land later, so it still gets one final lookup before giving up.
pub(super) const PENDING_EXIT_EVIDENCE_MS: u64 = 2_000;

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
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pending: bool,
}

impl CountedExit {
    fn new(launch: &HostedLaunch, status: HostedExitEvidenceStatus, observed_at_ms: u64) -> Self {
        let (evidence, pending) = match status {
            HostedExitEvidenceStatus::Exited(evidence) => (Some(evidence), false),
            HostedExitEvidenceStatus::Pending => (None, true),
            HostedExitEvidenceStatus::Unavailable => (None, false),
        };
        Self {
            stream: launch.stream.clone(),
            attempt: launch.attempt,
            observed_at_ms,
            evidence,
            pending,
        }
    }
}

pub(super) struct FailedResumeExit {
    launch: HostedLaunch,
    exit: CountedExit,
}

impl FailedResumeExit {
    pub(super) fn save_after_unspawned_fallback<S: StateStore>(
        self,
        store: &S,
        record: &HostedSessionRecord,
        state: &mut HostedRespawnState,
        by: &str,
    ) -> Result<(), LifecycleError> {
        let incident = &mut state.exit_evidence;
        if let Some(generation) = &record.hydration_generation
            && incident.launch.as_ref() == Some(&self.launch)
            && incident.push_counted_exit(self.exit)
        {
            // This resume was the only child in the attempt. Carry its exact stream
            // forward with the fallback generation in the same write as its exit.
            incident.launch = Some(HostedLaunch {
                generation: generation.clone(),
                ..self.launch
            });
            persist_counted_exit(store, record, state, by)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostedExitIncident {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    launch: Option<HostedLaunch>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    exits: Vec<CountedExit>,
}

pub(super) enum RecoveryExitEvidence {
    Pending,
    Ready {
        clean_exit: bool,
        resolution: Option<ResolvedPendingExit>,
    },
}

pub(super) struct ResolvedPendingExit {
    pub(super) observed_at_ms: u64,
    epoch: u64,
    detail: String,
}

impl ResolvedPendingExit {
    pub(super) fn log(&self, hosted_id: &str) {
        eprintln!(
            "hub: warning ({hosted_id}): resolved hosted child exit at epoch {}. {}",
            self.epoch, self.detail,
        );
    }
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

    pub(super) fn bound_stream(&self, record: &HostedSessionRecord) -> Option<&StreamId> {
        self.matching_launch(record).map(|launch| &launch.stream)
    }

    pub(super) fn prune_fast_exits(&mut self, fast_attempts: &[u64], now: u64) {
        if fast_attempts.is_empty()
            && let [older @ .., latest] = self.exits.as_mut_slice()
            && let LaunchAttempt::Fast(at) = latest.attempt
            && now.saturating_sub(at) >= AUTO_RESPAWN_WINDOW_MS
            && now.saturating_sub(latest.observed_at_ms) < AUTO_RESPAWN_WINDOW_MS
        {
            // A fresh exit from a launch whose fast reservation expired starts the new burst.
            latest.attempt = LaunchAttempt::Baseline;
            let older_count = older.len();
            self.exits.drain(..older_count);
        }
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
    ) -> HostedExitEvidenceStatus {
        let Some(stream) = self.bound_stream(record) else {
            return HostedExitEvidenceStatus::Unavailable;
        };
        if let Some(counted) = self.exits.iter().find(|entry| entry.stream == *stream) {
            return if counted.pending {
                HostedExitEvidenceStatus::Pending
            } else {
                counted.evidence.clone().map_or(
                    HostedExitEvidenceStatus::Unavailable,
                    HostedExitEvidenceStatus::Exited,
                )
            };
        }
        capture_exact(runtime, stream, &record.session_id)
    }

    pub(super) fn capture_failed_resume(
        &self,
        runtime: &dyn SessionRuntime,
        record: &HostedSessionRecord,
    ) -> Option<FailedResumeExit> {
        let launch = self.matching_launch(record)?;
        let observed_at_ms = current_respawn_clock_ms();
        let exit = CountedExit::new(launch, self.capture(runtime, record), observed_at_ms);
        Some(FailedResumeExit {
            launch: launch.clone(),
            exit,
        })
    }

    /// Resolve Pending for the current launch, retaining its original observation time.
    pub(super) fn refresh_for_recovery(
        &mut self,
        runtime: &dyn SessionRuntime,
        record: &HostedSessionRecord,
        now: u64,
    ) -> RecoveryExitEvidence {
        use RecoveryExitEvidence::{Pending, Ready};

        let no_evidence = Ready {
            clean_exit: false,
            resolution: None,
        };
        let Some(stream) = self.bound_stream(record).cloned() else {
            return no_evidence;
        };
        let Some(exit) = self.exits.last_mut().filter(|exit| exit.stream == stream) else {
            return no_evidence;
        };
        let mut resolution = None;
        if exit.pending {
            let elapsed = now.checked_sub(exit.observed_at_ms);
            if elapsed == Some(0) {
                return Pending;
            }
            if let HostedExitEvidenceStatus::Exited(evidence) =
                capture_exact(runtime, &exit.stream, &record.session_id)
            {
                exit.evidence = Some(evidence);
            } else if elapsed.is_some_and(|ms| ms < PENDING_EXIT_EVIDENCE_MS) {
                return Pending;
            }
            exit.pending = false;
            resolution = Some(ResolvedPendingExit {
                observed_at_ms: exit.observed_at_ms,
                epoch: exit.stream.epoch.0,
                detail: exit
                    .evidence
                    .as_ref()
                    .map_or_else(|| "evidence: unavailable".into(), exit_status),
            });
        }
        // A resolved clean exit still applies if a prior stopped-record write failed.
        Ready {
            clean_exit: exit.evidence.as_ref().is_some_and(is_clean_exit),
            resolution,
        }
    }

    fn count_exit(
        &mut self,
        record: &HostedSessionRecord,
        status: HostedExitEvidenceStatus,
        now: u64,
    ) -> bool {
        let Some(launch) = self.matching_launch(record) else {
            return false;
        };
        self.push_counted_exit(CountedExit::new(launch, status, now))
    }

    fn push_counted_exit(&mut self, exit: CountedExit) -> bool {
        if self.exits.iter().any(|entry| entry.stream == exit.stream) {
            return false;
        }
        if self.exits.len() >= MAX_EXITS {
            self.exits.drain(..self.exits.len() - MAX_EXITS + 1);
        }
        self.exits.push(exit);
        true
    }
}

fn capture_exact(
    runtime: &dyn SessionRuntime,
    stream: &StreamId,
    hosted_id: &str,
) -> HostedExitEvidenceStatus {
    match runtime.hosted_exit_evidence(stream, hosted_id) {
        HostedExitEvidenceStatus::Exited(evidence)
            if evidence.stream == *stream
                && evidence.terminal_tail.len() <= fl_host_proto::MAX_HOSTED_EXIT_TAIL_BYTES
                && evidence.terminal_tail.lines().count()
                    <= fl_host_proto::MAX_HOSTED_EXIT_TAIL_LINES
                && evidence
                    .terminal_tail
                    .chars()
                    .all(|ch| !ch.is_control() || matches!(ch, '\n' | '\t')) =>
        {
            HostedExitEvidenceStatus::Exited(evidence)
        }
        HostedExitEvidenceStatus::Pending => HostedExitEvidenceStatus::Pending,
        _ => HostedExitEvidenceStatus::Unavailable,
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
    status: HostedExitEvidenceStatus,
    observed_at_ms: u64,
    by: &str,
) -> Result<(), LifecycleError> {
    if state
        .exit_evidence
        .count_exit(record, status, observed_at_ms)
    {
        persist_counted_exit(store, record, state, by)?;
    }
    Ok(())
}

fn persist_counted_exit<S: StateStore>(
    store: &S,
    record: &HostedSessionRecord,
    state: &HostedRespawnState,
    by: &str,
) -> Result<(), LifecycleError> {
    save_hosted_respawn_state(store, &record.session_id, state, by)?;
    if let Some(exit) = state.exit_evidence.exits.last() {
        let detail = match &exit.evidence {
            Some(evidence) => exit_status(evidence),
            None if exit.pending => "evidence: pending".into(),
            None => "evidence: unavailable".into(),
        };
        eprintln!(
            "hub: warning ({}): counted hosted child exit at epoch {}. {detail}",
            record.session_id, exit.stream.epoch.0,
        );
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

pub(super) fn is_clean_exit(evidence: &HostedExitEvidence) -> bool {
    evidence.exit.code == Some(0) && evidence.exit.signal.is_none()
}

fn complete_failure(evidence: &HostedExitEvidence) -> bool {
    match (evidence.exit.code, evidence.exit.signal) {
        (Some(code), None) => code != 0,
        (None, Some(signal)) => signal > 0,
        _ => false,
    }
}

fn complete_launch_set(state: &HostedRespawnState) -> bool {
    let exits = &state.exit_evidence.exits;
    let fast = &state.respawn_timestamps_ms;
    let slow = state.recovery_attempts;
    if exits.len() < 2 || exits.len() != 1 + fast.len() + usize::from(slow) {
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
    let exits = &state.exit_evidence.exits;
    let mut available = exits.iter().filter_map(|entry| entry.evidence.as_ref());
    let statuses_match =
        complete_failure(evidence) && available.clone().all(|other| other.exit == evidence.exit);
    let all_available = available.clone().count() == exits.len() && complete_launch_set(state);
    let output_differs = available.any(|other| other.terminal_tail != evidence.terminal_tail);
    let assessment = match (statuses_match, output_differs, all_available) {
        (true, false, true) => "deterministic launch failure",
        (true, true, true) => "exit status identical, output differs",
        (true, true, false) => "exit status identical, output differs (available evidence only)",
        _ => "mixed or incomplete launch evidence",
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
    fn hosted_exit_evidence_restarts_the_burst_after_a_long_lived_fast_launch() {
        let mut state = HostedRespawnState::default();
        fn count(
            state: &mut HostedRespawnState,
            epoch: u64,
            attempt: LaunchAttempt,
            now: u64,
            tail: &str,
        ) {
            let failure = evidence(epoch, Some(1), None, tail);
            state.exit_evidence.exits.push(CountedExit {
                stream: failure.stream.clone(),
                attempt,
                observed_at_ms: now,
                evidence: Some(failure),
                pending: false,
            });
        }

        count(&mut state, 1, LaunchAttempt::Baseline, 1_000, "old failure");
        recovery::reserve_attempt(&mut state, 1_000);
        let new_burst = 1_000 + AUTO_RESPAWN_WINDOW_MS + 1;
        count(
            &mut state,
            2,
            LaunchAttempt::Fast(1_000),
            new_burst,
            "repeat failure",
        );
        for epoch in 3..=5 {
            let due = state.next_allowed_at_ms.unwrap().max(new_burst);
            assert_eq!(
                recovery::classify(&mut state, STOP_REASON_RUNTIME_GONE, due),
                recovery::Decision::FastLaunch
            );
            recovery::reserve_attempt(&mut state, due);
            count(
                &mut state,
                epoch,
                LaunchAttempt::Fast(due),
                due + 1,
                "repeat failure",
            );
        }
        let now = state.respawn_timestamps_ms[2] + 1;
        assert_eq!(
            recovery::classify(&mut state, STOP_REASON_RUNTIME_GONE, now),
            recovery::Decision::Wait
        );
        assert!(state.recovery_started_at_ms.is_some());
        assert!(summary(&state).contains("deterministic launch failure"));
        assert!(!summary(&state).contains("repeat failure"));
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
                        pending: false,
                    },
                    CountedExit {
                        stream: fast.stream.clone(),
                        attempt: LaunchAttempt::Fast(2),
                        observed_at_ms: 3,
                        evidence: Some(fast),
                        pending: false,
                    },
                ],
            },
            ..Default::default()
        };
        assert!(summary(&state).contains("deterministic launch failure"));
        assert!(!summary(&state).contains(fixed_tail));
        let differing = evidence(2, Some(1), None, "another terminal line");
        state.exit_evidence.exits[1].evidence = Some(differing);
        let changed = summary(&state);
        assert!(changed.contains("exit status identical, output differs"));
        assert!(!changed.contains("available evidence only"));
        assert!(!changed.contains("another terminal line"));
        state.recovery_attempts = 1;
        assert!(summary(&state).contains("available evidence only"));
        state.recovery_attempts = 0;
        for second in [Some(evidence(2, None, Some(1), fixed_tail)), None] {
            state.exit_evidence.exits[1].evidence = second;
            assert!(summary(&state).contains("mixed or incomplete launch evidence"));
        }
        state.exit_evidence.exits[0].evidence = None;
        assert_eq!(summary(&state), "evidence: unavailable");
    }
}
