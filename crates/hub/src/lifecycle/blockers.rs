//! Read-only monitoring for hosted startup blockers.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use fl_host_proto::{
    AsyncHostClient, ChildRecord, ChildState, DEFAULT_MAX_SESSIONS, HostEndpoint, HostError,
    SessionMode, SessionSummary, StreamId,
};
use fl_watch::{HostedBlock, HostedBlockReason};
use fl_wire::dispatcher::{HostedScreenBlocker, classify_hosted_screen_blocker};

use super::{DEFAULT_NODE_ID, HostedSessionRecord, HydrationStatus, STATUS_RUNNING};

const POLL_INTERVAL: Duration = Duration::from_secs(30);
const IPC_BUDGET: Duration = Duration::from_millis(250);
const MAX_CAPTURES: usize = 16;
// Allow a full Host rotation plus one poll of slack for lifecycle reconciliation.
const EXIT_EVIDENCE_AGE: Duration =
    POLL_INTERVAL.saturating_mul((DEFAULT_MAX_SESSIONS.div_ceil(MAX_CAPTURES) + 1) as u32);
const HYDRATION_OVERDUE_MS: u64 = 3 * 60 * 1_000;
const ACTOR: &str = "hub-hosted-blockers";

#[derive(Default)]
pub(crate) struct Sampler {
    next_poll: Option<Instant>,
    progress: BTreeMap<String, (StreamId, u64)>,
    last_capture: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScreenEvidence {
    Unknown,
    Clear,
    Blocked(HostedBlockReason),
}

impl ScreenEvidence {
    fn classify(vendor: &str, contents: &str) -> Self {
        match classify_hosted_screen_blocker(vendor, contents) {
            Some(HostedScreenBlocker::VendorSignIn) => {
                Self::Blocked(HostedBlockReason::VendorSignIn)
            }
            Some(HostedScreenBlocker::BlockingDialog) => {
                Self::Blocked(HostedBlockReason::BlockingDialog)
            }
            None if contents.trim().is_empty() => Self::Unknown,
            None => Self::Clear,
        }
    }
}

#[derive(Clone)]
struct Observation {
    started_at_ms: u64,
    screen: ScreenEvidence,
    stream: StreamId,
    pid: u32,
    checked_at: Instant,
    observed_at_ms: u64,
}

impl Observation {
    fn new(session: &SessionSummary, child: &ChildRecord, screen: ScreenEvidence) -> Self {
        Self {
            started_at_ms: child.started_at_ms,
            screen,
            stream: session.stream.clone(),
            pid: child.pid,
            checked_at: Instant::now(),
            observed_at_ms: super::now_ms(),
        }
    }
}

#[derive(Debug)]
struct RecentBlock {
    stream: StreamId,
    pid: u32,
    generation: Option<String>,
    updated_at_ms: u64,
    block: HostedBlock,
    checked_at: Instant,
}

/// Completed PTYs release their rendered screen and cannot answer ownership checks.
#[derive(Debug, Default)]
pub(crate) struct ExitEvidence(Mutex<BTreeMap<String, RecentBlock>>);

impl ExitEvidence {
    fn remember(&self, record: &HostedSessionRecord, observation: &Observation) {
        let ScreenEvidence::Blocked(reason) = observation.screen else {
            return;
        };
        let Some(block) = record.block.filter(|block| block.reason == reason) else {
            return;
        };
        let mut entries = self.0.lock().unwrap_or_else(|err| err.into_inner());
        entries.insert(
            record.session_id.clone(),
            RecentBlock {
                stream: observation.stream.clone(),
                pid: observation.pid,
                generation: record.hydration_generation.clone(),
                updated_at_ms: record.updated_at_ms,
                block,
                checked_at: observation.checked_at,
            },
        );
    }

    fn clear(&self, record: &HostedSessionRecord, observation: &Observation) {
        let mut entries = self.0.lock().unwrap_or_else(|err| err.into_inner());
        if entries
            .get(&record.session_id)
            .is_some_and(|entry| entry.stream == observation.stream && entry.pid == observation.pid)
        {
            entries.remove(&record.session_id);
        }
    }

    fn retain_current(&self, now: Instant, progress: &BTreeMap<String, (StreamId, u64)>) {
        self.0
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .retain(|id, entry| {
                now.saturating_duration_since(entry.checked_at) <= EXIT_EVIDENCE_AGE
                    && progress
                        .get(id)
                        .is_none_or(|(stream, _)| stream == &entry.stream)
            });
    }

    fn matches(&self, record: &HostedSessionRecord, block: HostedBlock, now: Instant) -> bool {
        self.0
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .get(&record.session_id)
            .is_some_and(|entry| {
                now.saturating_duration_since(entry.checked_at) <= EXIT_EVIDENCE_AGE
                    && record.host_session_id.as_deref() == Some(entry.stream.session.as_str())
                    && record.runtime_leader_pid.or(record.pid) == Some(entry.pid)
                    && entry.generation == record.hydration_generation
                    && entry.updated_at_ms == record.updated_at_ms
                    && entry.block == block
            })
    }
}

#[cfg(test)]
pub(super) fn remember_test_block(evidence: &ExitEvidence, record: &HostedSessionRecord) {
    let test_record = tests::summary(record);
    let (session, child) = running_session(record, std::slice::from_ref(&test_record)).unwrap();
    let reason = record.block.unwrap().reason;
    let sample = Observation::new(session, child, ScreenEvidence::Blocked(reason));
    evidence.remember(record, &sample);
}

pub(super) fn blocked_at_exit(
    record: &HostedSessionRecord,
    evidence: Option<&ExitEvidence>,
) -> bool {
    record.active_block().is_some_and(|block| {
        // A missing hydration receipt leaves the seat's ability to run unknown.
        matches!(
            block.reason,
            HostedBlockReason::VendorSignIn | HostedBlockReason::BlockingDialog
        ) && evidence.is_some_and(|evidence| evidence.matches(record, block, Instant::now()))
    })
}

impl Sampler {
    fn poll_due(&mut self, now: Instant) -> bool {
        if self.next_poll.is_some_and(|next| now < next) {
            return false;
        }
        self.next_poll = Some(now + POLL_INTERVAL);
        true
    }

    /// Runs in the existing blocking lifecycle worker after ordinary reconciliation.
    pub(crate) fn reconcile(&mut self, state: &crate::HubState) {
        let env = crate::api::lifecycle_env(state);
        if !env.spawn_runtime || !self.poll_due(Instant::now()) {
            return;
        }
        let local_node = crate::node_id_from_hostname(&crate::local_hostname());
        let records: Vec<_> = match state.try_open_lifecycle_store() {
            Ok(store) => match super::list_hosted_sessions(&*store) {
                Ok(records) => records
                    .into_iter()
                    .filter(|record| candidate(record, &local_node))
                    .collect(),
                Err(_) => return,
            },
            Err(_) => return,
        };
        if records.is_empty() {
            self.progress.clear();
            state
                .hosted_blocker_evidence
                .retain_current(Instant::now(), &self.progress);
            return;
        }
        let endpoint = HostEndpoint::discover().ok();
        let mut observations = vec![None; records.len()];
        if let Some(endpoint) = &endpoint {
            tokio::runtime::Handle::current().block_on(async {
                // An unresponsive Host cannot hold up the next lifecycle pass.
                let _ = tokio::time::timeout(
                    IPC_BUDGET,
                    self.observe(endpoint, &records, &mut observations),
                )
                .await;
            });
        }
        let now_ms = super::now_ms();
        let evidence = &state.hosted_blocker_evidence;
        evidence.retain_current(Instant::now(), &self.progress);
        for (record, observation) in records.iter().zip(observations.iter()) {
            let observation = observation.as_ref();
            if let Some(observation) =
                observation.filter(|sample| sample.screen == ScreenEvidence::Clear)
                && record.block.is_some()
            {
                // Clear risk of misclassified exits even if the durable flag cannot be written yet.
                evidence.clear(record, observation);
            }
            if block_reason(record) == next_reason(record, observation, now_ms) {
                if let Some(observation) = observation {
                    evidence.remember(record, observation);
                }
                continue;
            }
            super::with_background_lifecycle_fence(&env, record, ACTOR, |env| {
                let store = state.try_open_lifecycle_store()?;
                let Some(mut fresh) =
                    super::reload_background_session(&*store, env, &record.session_id)?
                else {
                    return Ok(());
                };
                if !same_launch(record, &fresh, &local_node) {
                    return Ok(());
                }
                super::promote_confirmed_hydration(&*store, &mut fresh)?;
                let reason = next_reason(&fresh, observation, now_ms);
                if block_reason(&fresh) != reason {
                    fresh.block = reason.map(|reason| HostedBlock {
                        reason,
                        observed_at_ms: observation.map_or(now_ms, |sample| sample.observed_at_ms),
                    });
                    super::promote_confirmed_hydration(&*store, &mut fresh)?;
                    super::persist_hosted_session(&*store, &fresh, ACTOR)?;
                }
                if let Some(observation) = observation {
                    evidence.remember(&fresh, observation);
                }
                Ok(())
            });
        }
    }

    async fn observe(
        &mut self,
        endpoint: &HostEndpoint,
        records: &[HostedSessionRecord],
        observations: &mut [Option<Observation>],
    ) {
        let Ok(mut client) = AsyncHostClient::connect(endpoint).await else {
            return;
        };
        let Ok(sessions) = client.list_sessions().await else {
            return;
        };
        let eligible = self.eligible(records, &sessions);
        // Candidates are sorted by record ID. Continue after the last attempted capture
        // so every quiet seat gets a turn even with a full Host.
        let start = eligible.partition_point(|(index, _, _)| {
            self.last_capture
                .as_ref()
                .is_some_and(|last| &records[*index].session_id <= last)
        });
        for &(index, session, child) in eligible
            .iter()
            .skip(start)
            .chain(&eligible[..start])
            .take(MAX_CAPTURES)
        {
            let record = &records[index];
            self.last_capture = Some(record.session_id.clone());
            match client
                .check_session_owner(session.stream.clone(), &record.session_id)
                .await
            {
                Ok(true) => {}
                Ok(false) => continue,
                Err(_) => break,
            }
            // Verified liveness is enough to detect a hydration timeout if the screen
            // is unavailable. It cannot renew the evidence of a previous screen block.
            let mut observation = Observation::new(session, child, ScreenEvidence::Unknown);
            observations[index] = Some(observation.clone());
            match client.capture_rendered(session.stream.clone()).await {
                Ok(screen) if screen.stream == session.stream => {
                    observation.screen = ScreenEvidence::classify(&record.vendor, &screen.contents);
                    observations[index] = Some(observation);
                }
                Ok(_) | Err(HostError::StaleEpoch { .. } | HostError::PtyEpochEvicted { .. }) => {
                    observations[index] = None;
                }
                Err(HostError::Unsupported { .. }) => {}
                Err(_) => break,
            }
        }
    }

    fn eligible<'a>(
        &mut self,
        records: &[HostedSessionRecord],
        sessions: &'a [SessionSummary],
    ) -> Vec<(usize, &'a SessionSummary, &'a ChildRecord)> {
        let mut current = BTreeMap::new();
        let mut eligible = Vec::new();
        for (index, record) in records.iter().enumerate() {
            let Some((session, child)) = running_session(record, sessions) else {
                continue;
            };
            let watermark = (session.stream.clone(), session.latest);
            let quiet = self.progress.get(&record.session_id) == Some(&watermark);
            current.insert(record.session_id.clone(), watermark);
            // Advancing output can be a repaint of the same prompt.
            if record.hydration_status == HydrationStatus::Unconfirmed || quiet {
                eligible.push((index, session, child));
            }
        }
        self.progress = current;
        eligible
    }
}

fn candidate(record: &HostedSessionRecord, local_node: &str) -> bool {
    record.hosted
        && record.status.eq_ignore_ascii_case(STATUS_RUNNING)
        && (record.node_id == local_node || record.node_id == DEFAULT_NODE_ID)
        && super::vendor_has_runtime(&record.vendor)
        && record.host_session_id.is_some()
}

fn running_session<'a>(
    record: &HostedSessionRecord,
    sessions: &'a [SessionSummary],
) -> Option<(&'a SessionSummary, &'a ChildRecord)> {
    let id = record.host_session_id.as_deref()?;
    let mut matches = sessions.iter().filter_map(|session| match &session.state {
        ChildState::Running(child)
            if session.mode == SessionMode::Pty
                && session.stream.session.as_str() == id
                && child.session == session.stream.session
                && child.session_epoch == session.stream.epoch =>
        {
            Some((session, child))
        }
        _ => None,
    });
    let (session, child) = matches.next()?;
    (matches.next().is_none()
        && record
            .runtime_leader_pid
            .or(record.pid)
            .is_none_or(|pid| child.pid == pid))
    .then_some((session, child))
}

fn same_launch(old: &HostedSessionRecord, fresh: &HostedSessionRecord, local_node: &str) -> bool {
    candidate(fresh, local_node)
        && fresh.session_id == old.session_id
        && fresh.vendor == old.vendor
        && fresh.node_id == old.node_id
        && fresh.host_session_id == old.host_session_id
        && fresh.hydration_generation == old.hydration_generation
        && fresh.updated_at_ms == old.updated_at_ms
        && fresh.restart_count == old.restart_count
        && fresh.runtime_leader_pid == old.runtime_leader_pid
        && fresh.pid == old.pid
        && fresh.pid_start_time == old.pid_start_time
}

fn block_reason(record: &HostedSessionRecord) -> Option<HostedBlockReason> {
    record.block.as_ref().map(|block| block.reason)
}

fn hydration_started_at_ms(record: &HostedSessionRecord) -> Option<u64> {
    // Legacy records without a generation have no tracked acknowledgement to time out.
    let generation = record.hydration_generation.as_deref()?;
    if !super::is_valid_hydration_generation(generation) {
        return None;
    }
    let (seconds, nanos) = uuid::Uuid::parse_str(generation)
        .ok()?
        .get_timestamp()?
        .to_unix();
    Some(
        seconds
            .saturating_mul(1_000)
            .saturating_add(u64::from(nanos / 1_000_000)),
    )
}

fn next_reason(
    record: &HostedSessionRecord,
    observation: Option<&Observation>,
    now_ms: u64,
) -> Option<HostedBlockReason> {
    let previous = block_reason(record);
    let unconfirmed = record.hydration_status == HydrationStatus::Unconfirmed;
    let screen = observation.map_or(ScreenEvidence::Unknown, |observation| observation.screen);
    if let ScreenEvidence::Blocked(reason) = screen {
        return Some(reason);
    }
    if screen == ScreenEvidence::Unknown
        && previous != Some(HostedBlockReason::HydrationOverdue)
        && previous.is_some()
    {
        return previous;
    }
    if unconfirmed
        && let Some(generation_ms) = hydration_started_at_ms(record)
        && (previous == Some(HostedBlockReason::HydrationOverdue)
            || observation.is_some_and(|observation| {
                now_ms.saturating_sub(
                    observation
                        .started_at_ms
                        .max(generation_ms)
                        .max(record.updated_at_ms),
                ) >= HYDRATION_OVERDUE_MS
            }))
    {
        return Some(HostedBlockReason::HydrationOverdue);
    }
    None
}

#[cfg(test)]
mod tests {
    use fl_host_proto::{HostInstance, InputState, SessionEpoch, SessionId, SpawnKind};

    use super::*;

    const SIGN_IN: &str = "Welcome to Codex\n\n  › 1. Sign in with ChatGPT\n    2. Use an API key";

    fn record(index: usize) -> HostedSessionRecord {
        serde_json::from_value(serde_json::json!({
            "session_id": format!("codex:seat-{index:02}"),
            "persona": format!("seat-{index:02}"),
            "project": "ferroloop", "role": "assistant", "vendor": "codex",
            "node_id": DEFAULT_NODE_ID, "hosted": true, "status": STATUS_RUNNING,
            "host_session_id": format!("Seat-{index:02}.codex"),
            "restart_policy": "on-failure", "restart_count": 0,
            "hydration_dispatched": true, "hydration_status": "unconfirmed",
            "hydration_generation": "00000000-03e8-7000-8000-000000000001",
            "runtime_leader_pid": 10_000 + index as u32,
            "created_at_ms": 1000, "updated_at_ms": 1000,
        }))
        .unwrap()
    }

    pub(super) fn summary(record: &HostedSessionRecord) -> SessionSummary {
        let stream = StreamId {
            host: HostInstance::new([1; 16]),
            session: SessionId::new(record.host_session_id.as_deref().unwrap()).unwrap(),
            epoch: SessionEpoch(1),
        };
        SessionSummary {
            state: ChildState::Running(ChildRecord {
                pid: record.runtime_leader_pid.unwrap(),
                session: stream.session.clone(),
                session_epoch: stream.epoch,
                kind: SpawnKind::Pty,
                input: InputState::Writable,
                started_at_ms: 1000,
                exit: None,
                consumer_stalled: false,
            }),
            stream,
            mode: SessionMode::Pty,
            acked: 0,
            latest: 10,
            input: InputState::Writable,
        }
    }

    #[test]
    fn samples_unconfirmed_and_quiet_seats_at_a_fixed_rate() {
        let mut sampler = Sampler::default();
        let now = Instant::now();
        assert!(sampler.poll_due(now));
        assert!(!sampler.poll_due(now + Duration::from_secs(15)));
        assert!(sampler.poll_due(now + POLL_INTERVAL));

        let mut records = vec![record(0), record(1), record(2)];
        records[1].hydration_status = HydrationStatus::Confirmed;
        records[2].hydration_status = HydrationStatus::Confirmed;
        let mut sessions: Vec<_> = records.iter().map(summary).collect();
        let eligible = |sampler: &mut Sampler, sessions: &[SessionSummary]| -> Vec<_> {
            sampler
                .eligible(&records, sessions)
                .into_iter()
                .map(|entry| entry.0)
                .collect()
        };
        assert_eq!(eligible(&mut sampler, &sessions), [0]);
        sessions[2].latest += 1;
        assert_eq!(eligible(&mut sampler, &sessions), [0, 1]);
    }

    #[test]
    fn rejects_replaced_ambiguous_and_remote_sessions() {
        let record = record(0);
        assert!(candidate(&record, "local"));
        let mut other = record.clone();
        other.node_id = "another-node".into();
        assert!(!candidate(&other, "local"));
        let session = summary(&record);
        assert!(running_session(&record, std::slice::from_ref(&session)).is_some());
        assert!(running_session(&record, &[session.clone(), session.clone()]).is_none());
        other.node_id = record.node_id.clone();
        other.runtime_leader_pid = Some(9999);
        assert!(running_session(&other, &[session]).is_none());
        assert!(!same_launch(&record, &other, "local"));
        other.runtime_leader_pid = record.runtime_leader_pid;
        other.hydration_generation = Some("00000000-03e9-7000-8000-000000000001".into());
        assert!(!same_launch(&record, &other, "local"));
    }

    #[test]
    fn tracks_hydration_timeout_and_preserves_known_block_on_unknown_screen() {
        let mut record = record(0);
        assert!(record.block.is_none());
        let session = summary(&record);
        let (session, child) = running_session(&record, std::slice::from_ref(&session)).unwrap();
        let observe = |screen| Observation::new(session, child, screen);
        let now = 1000 + HYDRATION_OVERDUE_MS;
        assert_eq!(next_reason(&record, None, now), None);
        assert_eq!(
            next_reason(&record, Some(&observe(ScreenEvidence::Unknown)), now - 1),
            None
        );
        assert_eq!(
            next_reason(&record, Some(&observe(ScreenEvidence::Unknown)), now),
            Some(HostedBlockReason::HydrationOverdue)
        );
        let generation = record
            .hydration_generation
            .replace("00000003-0000-7000-8000-000000000001".into());
        assert_eq!(
            next_reason(&record, Some(&observe(ScreenEvidence::Unknown)), now),
            None
        );
        record.hydration_generation = None;
        assert_eq!(
            next_reason(&record, Some(&observe(ScreenEvidence::Unknown)), now),
            None
        );
        record.hydration_generation = generation;
        let sign_in = ScreenEvidence::classify("codex", SIGN_IN);
        assert_eq!(
            next_reason(&record, Some(&observe(sign_in)), now),
            Some(HostedBlockReason::VendorSignIn)
        );
        record.block = Some(HostedBlock {
            reason: HostedBlockReason::VendorSignIn,
            observed_at_ms: 1100,
        });
        let blank = ScreenEvidence::classify("codex", " \n");
        assert_eq!(
            next_reason(&record, Some(&observe(blank)), now),
            Some(HostedBlockReason::VendorSignIn)
        );
        let ready = ScreenEvidence::classify("codex", "› Ready for a prompt");
        assert_eq!(next_reason(&record, Some(&observe(ready)), now - 1), None);
        assert_eq!(
            next_reason(&record, Some(&observe(ready)), now),
            Some(HostedBlockReason::HydrationOverdue)
        );
        record.block.as_mut().unwrap().reason = HostedBlockReason::HydrationOverdue;
        record.hydration_status = HydrationStatus::Confirmed;
        assert_eq!(next_reason(&record, None, now), None);
    }

    #[test]
    fn stale_or_replaced_evidence_cannot_suppress_crash_recovery() {
        let mut record = record(0);
        record.hydration_status = HydrationStatus::Confirmed;
        let block = HostedBlock {
            reason: HostedBlockReason::BlockingDialog,
            observed_at_ms: 1_001,
        };
        record.block = Some(block);
        let mut session = summary(&record);
        let (initial, child) = running_session(&record, std::slice::from_ref(&session)).unwrap();
        let sample = Observation::new(initial, child, ScreenEvidence::Blocked(block.reason));
        let evidence = ExitEvidence::default();
        evidence.remember(&record, &sample);
        assert!(evidence.matches(&record, block, sample.checked_at + EXIT_EVIDENCE_AGE));
        assert!(!evidence.matches(
            &record,
            block,
            sample.checked_at + EXIT_EVIDENCE_AGE + Duration::from_millis(1),
        ));
        record.runtime_leader_pid = Some(sample.pid + 1);
        assert!(!evidence.matches(&record, block, sample.checked_at));
        record.runtime_leader_pid = Some(sample.pid);
        session.stream.epoch.0 += 1;
        evidence.retain_current(
            sample.checked_at,
            &BTreeMap::from([(record.session_id.clone(), (session.stream, session.latest))]),
        );
        assert!(!evidence.matches(&record, block, sample.checked_at));
    }

    #[test]
    fn prompt_repaint_keeps_blocker_until_quiet_capture_confirms_recovery() {
        let mut record = record(0);
        record.hydration_status = HydrationStatus::Confirmed;
        let block = HostedBlock {
            reason: HostedBlockReason::VendorSignIn,
            observed_at_ms: 1_001,
        };
        record.block = Some(block);
        let mut sessions = [summary(&record)];
        let records = std::slice::from_ref(&record);
        let mut sampler = Sampler::default();
        let evidence = ExitEvidence::default();

        assert!(sampler.eligible(records, &sessions).is_empty());
        let eligible = sampler.eligible(records, &sessions);
        let [(_, session, child)] = eligible.as_slice() else {
            panic!("one quiet seat should be eligible");
        };
        let sign_in = ScreenEvidence::classify(&record.vendor, SIGN_IN);
        let before_repaint = Observation::new(session, child, sign_in);
        evidence.remember(&record, &before_repaint);

        // A resize can repaint the same prompt and advance the journal.
        sessions[0].latest += 1;
        assert!(sampler.eligible(records, &sessions).is_empty());
        assert_eq!(next_reason(&record, None, 2_000), Some(block.reason));
        evidence.retain_current(before_repaint.checked_at, &sampler.progress);
        assert!(blocked_at_exit(&record, Some(&evidence)));

        let eligible = sampler.eligible(records, &sessions);
        let [(_, session, child)] = eligible.as_slice() else {
            panic!("the seat should be eligible again after it goes quiet");
        };
        let ready = ScreenEvidence::classify(&record.vendor, "› Ready for a prompt");
        let recovered = Observation::new(session, child, ready);
        assert_eq!(next_reason(&record, Some(&recovered), 2_000), None);
        evidence.clear(&record, &recovered);
        assert!(!blocked_at_exit(&record, Some(&evidence)));
    }

    #[test]
    fn reused_generation_gets_a_full_timeout_after_redispatch() {
        let mut record = record(0);
        record.updated_at_ms = 10 * HYDRATION_OVERDUE_MS;
        let session = summary(&record);
        let (session, child) = running_session(&record, std::slice::from_ref(&session)).unwrap();
        let sample = Observation::new(session, child, ScreenEvidence::Clear);
        let observation = Some(&sample);
        assert_eq!(
            next_reason(
                &record,
                observation,
                record.updated_at_ms + HYDRATION_OVERDUE_MS - 1
            ),
            None,
        );
        assert_eq!(
            next_reason(
                &record,
                observation,
                record.updated_at_ms + HYDRATION_OVERDUE_MS
            ),
            Some(HostedBlockReason::HydrationOverdue),
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn private_host_fixture_samples_bounded_batches() {
        use fl_host_proto::{
            ExtensionLimits, ExtensionRequest, ExtensionResponse, HostConfig, HostListener,
            HostRequest, HostResponse, RENDERED_PTY_FEATURE, RenderedScreen, SESSION_OWNER_FEATURE,
        };
        use std::os::unix::fs::PermissionsExt;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let dir = tempfile::TempDir::new().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = dir.path().join("blockers.sock");
        let endpoint = HostEndpoint::unix(&path).unwrap();
        let config = HostConfig::new(endpoint.clone());
        let mut listener = HostListener::bind(config).await.unwrap();
        let _fixture = fl_host_proto::register_hub_test_host_fixture(&path).unwrap();
        let records: Vec<_> = (0..2 * MAX_CAPTURES).map(record).collect();
        let summaries: Vec<_> = records.iter().map(summary).collect();
        let requests = Arc::new(AtomicUsize::new(0));
        let server_requests = requests.clone();
        let server = tokio::spawn(async move {
            while let Ok(mut conn) = listener.accept().await {
                while let Ok(HostRequest::Extended { id, request }) = conn.recv().await {
                    server_requests.fetch_add(1, Ordering::Relaxed);
                    let response = match request {
                        ExtensionRequest::ListSessions => ExtensionResponse::Sessions {
                            records: summaries.clone(),
                        },
                        ExtensionRequest::Capabilities => ExtensionResponse::Capabilities {
                            host: HostInstance::new([1; 16]),
                            features: SESSION_OWNER_FEATURE | RENDERED_PTY_FEATURE,
                            limits: ExtensionLimits::default(),
                        },
                        ExtensionRequest::CheckSessionOwner { stream, hosted_id } => {
                            let persona = hosted_id.strip_prefix("codex:").unwrap();
                            let host_name = stream.session.as_str().to_ascii_lowercase();
                            ExtensionResponse::SessionOwner {
                                matches: host_name.strip_suffix(".codex") == Some(persona),
                            }
                        }
                        ExtensionRequest::CaptureRendered { stream } => {
                            ExtensionResponse::RenderedCapture {
                                screen: RenderedScreen {
                                    schema: 1,
                                    stream,
                                    contents: SIGN_IN.into(),
                                    cursor_row: 4,
                                    cursor_col: 20,
                                    rows: 24,
                                    cols: 80,
                                    generation: 1,
                                    output_seq: 10,
                                    output_bytes: SIGN_IN.len() as u64,
                                },
                            }
                        }
                        _ => panic!("unexpected sampler request"),
                    };
                    if conn
                        .respond(&HostResponse::Extended { id, response })
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            }
        });
        let mut sampler = Sampler::default();
        let batch = |batch| (batch * MAX_CAPTURES..(batch + 1) * MAX_CAPTURES).collect::<Vec<_>>();
        let mut elapsed = Duration::ZERO;
        const SWEEPS: usize = 10;
        for sweep in 0..SWEEPS {
            let start = Instant::now();
            let mut observations = vec![None; records.len()];
            sampler
                .observe(&endpoint, &records, &mut observations)
                .await;
            elapsed += start.elapsed();
            let sampled: Vec<_> = observations
                .iter()
                .enumerate()
                .filter_map(|(index, item)| {
                    item.as_ref()
                        .filter(|item| {
                            item.screen == ScreenEvidence::Blocked(HostedBlockReason::VendorSignIn)
                        })
                        .map(|_| index)
                })
                .collect();
            assert_eq!(sampled, batch(sweep % 2));
        }
        assert_eq!(
            requests.load(Ordering::Relaxed),
            SWEEPS * (1 + 4 * MAX_CAPTURES)
        );
        eprintln!(
            "Hosted blocker sampler fixture: {SWEEPS} sweeps, {MAX_CAPTURES} captures per sweep, {:?} mean IPC time",
            elapsed / SWEEPS as u32
        );
        server.abort();
    }
}
