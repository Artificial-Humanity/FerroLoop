//! Read-only monitoring for hosted startup blockers.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use fl_host_proto::{
    AsyncHostClient, ChildRecord, ChildState, HostEndpoint, HostError, SessionMode, SessionSummary,
    StreamId,
};
use fl_watch::{HostedBlock, HostedBlockReason};
use fl_wire::dispatcher::{HostedScreenBlocker, classify_hosted_screen_blocker};

use super::{DEFAULT_NODE_ID, HostedSessionRecord, HydrationStatus, STATUS_RUNNING};

const POLL_INTERVAL: Duration = Duration::from_secs(30);
const IPC_BUDGET: Duration = Duration::from_millis(250);
const MAX_CAPTURES: usize = 16;
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

#[derive(Clone, Copy)]
struct Observation {
    started_at_ms: u64,
    screen: ScreenEvidence,
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
        for (record, observation) in records.iter().zip(observations) {
            if block_reason(record) == next_reason(record, observation, now_ms) {
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
                        observed_at_ms: now_ms,
                    });
                    super::persist_hosted_session(&*store, &fresh, ACTOR)?;
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
            // Verified liveness is enough to detect a hydration timeout, even if the
            // screen cannot be read. A previous sign-in or dialog classification stays.
            observations[index] = Some(Observation {
                started_at_ms: child.started_at_ms,
                screen: ScreenEvidence::Unknown,
            });
            match client.capture_rendered(session.stream.clone()).await {
                Ok(screen) if screen.stream == session.stream => {
                    observations[index] = Some(Observation {
                        started_at_ms: child.started_at_ms,
                        screen: ScreenEvidence::classify(&record.vendor, &screen.contents),
                    });
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
            // An advancing journal sequence proves activity. Coalesced output can leave
            // it unchanged, so use it only to skip, and never to cache a classification.
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
    observation: Option<Observation>,
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
                now_ms.saturating_sub(observation.started_at_ms.max(generation_ms))
                    >= HYDRATION_OVERDUE_MS
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

    fn summary(record: &HostedSessionRecord) -> SessionSummary {
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
        let observe = |screen| {
            Some(Observation {
                started_at_ms: 1000,
                screen,
            })
        };
        let now = 1000 + HYDRATION_OVERDUE_MS;
        assert_eq!(next_reason(&record, None, now), None);
        assert_eq!(
            next_reason(&record, observe(ScreenEvidence::Unknown), now - 1),
            None
        );
        assert_eq!(
            next_reason(&record, observe(ScreenEvidence::Unknown), now),
            Some(HostedBlockReason::HydrationOverdue)
        );
        let generation = record
            .hydration_generation
            .replace("00000003-0000-7000-8000-000000000001".into());
        assert_eq!(
            next_reason(&record, observe(ScreenEvidence::Unknown), now),
            None
        );
        record.hydration_generation = None;
        assert_eq!(
            next_reason(&record, observe(ScreenEvidence::Unknown), now),
            None
        );
        record.hydration_generation = generation;
        let sign_in = ScreenEvidence::classify("codex", SIGN_IN);
        assert_eq!(
            next_reason(&record, observe(sign_in), now),
            Some(HostedBlockReason::VendorSignIn)
        );
        record.block = Some(HostedBlock {
            reason: HostedBlockReason::VendorSignIn,
            observed_at_ms: 1100,
        });
        let blank = ScreenEvidence::classify("codex", " \n");
        assert_eq!(
            next_reason(&record, observe(blank), now),
            Some(HostedBlockReason::VendorSignIn)
        );
        let ready = ScreenEvidence::classify("codex", "› Ready for a prompt");
        assert_eq!(next_reason(&record, observe(ready), now - 1), None);
        assert_eq!(
            next_reason(&record, observe(ready), now),
            Some(HostedBlockReason::HydrationOverdue)
        );
        record.block.as_mut().unwrap().reason = HostedBlockReason::HydrationOverdue;
        record.hydration_status = HydrationStatus::Confirmed;
        assert_eq!(next_reason(&record, None, now), None);
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
                    item.filter(|item| {
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
