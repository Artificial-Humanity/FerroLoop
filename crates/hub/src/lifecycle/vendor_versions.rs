//! Disjoint hosted launch receipts and host-local vendor version baselines.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use fl_core::vendor_adapter::VendorDaemonLaunchEvidence;
use fl_core::{RevisionId, VendorVersionRegistry};
use fl_host_proto::StreamId;
use sha2::{Digest, Sha256};

use super::*;

const LAUNCH_PREFIX: &str = "hosted-vendor-launch-";
const BASELINE_PREFIX: &str = "hosted-vendor-baseline-";
const GENERATION_PREFIX: &str = "hosted-version-gen-";
const QUEUE_PREFIX: &str = "hosted-version-queue-";
const ACTOR: &str = "hub-vendor-versions";
const OBSERVATIONS_PER_PASS: usize = 8;
const MAX_PENDING_LAUNCHES: usize = 64;
const STATE_UPDATE_ATTEMPTS: usize = 4;
const SETTLE_MS: u64 = 250;
const MAX_WRAPPER_CHILDREN: usize = 8;

static ADAPTERS: OnceLock<VendorVersionRegistry> = OnceLock::new();

pub fn set_global_vendor_version_adapters(
    registry: VendorVersionRegistry,
) -> &'static VendorVersionRegistry {
    ADAPTERS.get_or_init(|| registry)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostedVendorVersionComponent {
    Cli,
    Daemon,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostedVendorVersionChange {
    pub component: HostedVendorVersionComponent,
    pub previous: String,
    pub current: String,
}

/// Version evidence for one launch. An absent component is an unknown version.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostedVendorVersions {
    pub cli: Option<String>,
    pub daemon: Option<String>,
    pub daemon_expected: bool,
    pub changes: Vec<HostedVendorVersionChange>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct ProcessIdentity {
    pid: u32,
    start_ticks: u64,
}

impl ProcessIdentity {
    pub(super) fn capture(pid: u32) -> Option<Self> {
        let start_ticks = fl_wire::identity::proc_start_time_ticks(pid)?;
        Some(Self { pid, start_ticks })
    }

    fn is_current(&self) -> bool {
        fl_wire::identity::proc_start_time_ticks(self.pid) == Some(self.start_ticks)
    }

    fn executable(&self) -> Option<PathBuf> {
        if !self.is_current() {
            return None;
        }
        let path = std::fs::read_link(format!("/proc/{}/exe", self.pid)).ok()?;
        (path.is_absolute() && self.is_current()).then_some(path)
    }

    fn children(&self) -> Vec<Self> {
        use std::io::Read as _;

        let children = format!("/proc/{0}/task/{0}/children", self.pid);
        let Ok(file) = std::fs::File::open(children) else {
            return Vec::new();
        };
        let mut pids = String::new();
        if file.take(256).read_to_string(&mut pids).is_err() || pids.len() == 256 {
            return Vec::new();
        }
        pids.split_whitespace()
            .take(MAX_WRAPPER_CHILDREN)
            .filter_map(|pid| Self::capture(pid.parse().ok()?))
            .collect()
    }

    fn cli_version(
        &self,
        adapter: &dyn fl_core::VendorVersionAdapter,
        wrapper_levels: u8,
    ) -> Option<String> {
        let executable = self.executable()?;
        if let Some(version) = adapter.version_from_executable(&executable) {
            return Some(version);
        }
        if wrapper_levels == 0 || !is_launcher_image(&executable) {
            return None;
        }
        let mut versions = self
            .children()
            .into_iter()
            .filter(|child| {
                fl_wire::identity::pid_is_self_or_ancestor(
                    child.pid,
                    self.pid,
                    Some(self.start_ticks),
                )
            })
            .filter_map(|child| child.cli_version(adapter, wrapper_levels - 1));
        let version = versions.next()?;
        // More than one candidate is ambiguous even if both report the same release.
        (versions.next().is_none() && self.is_current()).then_some(version)
    }
}

fn is_launcher_image(path: &Path) -> bool {
    matches!(
        path.file_name().and_then(|name| name.to_str()),
        Some("ash" | "bash" | "bun" | "dash" | "env" | "node" | "nodejs" | "sh" | "zsh")
    )
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct DaemonEvidence {
    endpoint: VendorDaemonLaunchEvidence,
    peer: Option<ProcessIdentity>,
}

impl DaemonEvidence {
    pub(super) fn capture(endpoint: VendorDaemonLaunchEvidence) -> Self {
        let peer = endpoint.peer_pid.and_then(ProcessIdentity::capture);
        Self { endpoint, peer }
    }

    fn same_socket(&self) -> bool {
        self.endpoint.socket_identity.is_some()
            && VendorDaemonLaunchEvidence::socket_identity(&self.endpoint.socket)
                == self.endpoint.socket_identity
    }

    fn observe(
        &self,
        adapter: &dyn fl_core::VendorVersionAdapter,
        allow_rpc: bool,
    ) -> Option<String> {
        if !self.same_socket() {
            return None;
        }
        let known = self
            .peer
            .as_ref()
            .and_then(|peer| adapter.daemon_version_from_executable(&peer.executable()?));
        let version = known.or_else(|| {
            if !allow_rpc {
                return None;
            }
            match &self.peer {
                Some(peer) if peer.is_current() => {
                    let version = adapter
                        .observe_launched_daemon_version(&self.endpoint.config_home, peer.pid);
                    peer.is_current().then_some(version).flatten()
                }
                None if self.endpoint.peer_pid.is_none() => {
                    adapter.observe_daemon_version(&self.endpoint.config_home)
                }
                _ => None,
            }
        });
        self.same_socket().then_some(version).flatten()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LaunchEvidence {
    boot_id: Option<String>,
    leader: Option<ProcessIdentity>,
    daemon: Option<DaemonEvidence>,
    automatic: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LaunchReceipt {
    id: uuid::Uuid,
    captured_at_ms: u64,
    host: String,
    session_id: String,
    generation: String,
    stream: Option<StreamId>,
    vendor: String,
    daemon_expected: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    evidence: Option<LaunchEvidence>,
    #[serde(skip_serializing_if = "Option::is_none")]
    versions: Option<HostedVendorVersions>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct KnownVersion {
    launch: uuid::Uuid,
    version: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct VendorBaseline {
    cli: Option<KnownVersion>,
    daemon: Option<KnownVersion>,
    /// Checkpoint until the receipt is saved. A retry reuses the original comparison.
    last_applied: Option<(uuid::Uuid, HostedVendorVersions)>,
}

impl VendorBaseline {
    fn apply(
        &mut self,
        launch: uuid::Uuid,
        mut versions: HostedVendorVersions,
    ) -> HostedVendorVersions {
        if let Some((applied, versions)) = &self.last_applied
            && *applied == launch
        {
            return versions.clone();
        }
        let components = [
            (
                HostedVendorVersionComponent::Cli,
                &versions.cli,
                &mut self.cli,
            ),
            (
                HostedVendorVersionComponent::Daemon,
                &versions.daemon,
                &mut self.daemon,
            ),
        ];
        for (component, observed, last_known) in components {
            let Some(observed) = observed else {
                continue;
            };
            if let Some(known) = last_known {
                if known.launch >= launch {
                    continue;
                }
                if known.version != *observed {
                    versions.changes.push(HostedVendorVersionChange {
                        component,
                        previous: known.version.clone(),
                        current: observed.clone(),
                    });
                }
            }
            *last_known = Some(KnownVersion {
                launch,
                version: observed.clone(),
            });
        }
        self.last_applied = Some((launch, versions.clone()));
        versions
    }
}

fn local_host_key() -> &'static str {
    static HOST: OnceLock<String> = OnceLock::new();
    HOST.get_or_init(|| {
        let identity = std::fs::read_to_string("/etc/machine-id")
            .ok()
            .filter(|id| !id.trim().is_empty())
            .or_else(|| std::fs::read_to_string("/etc/hostname").ok())
            .unwrap_or_else(|| "workspace-local".into());
        digest(identity.trim().as_bytes())
    })
}

fn boot_id() -> Option<&'static str> {
    static BOOT: OnceLock<Option<String>> = OnceLock::new();
    BOOT.get_or_init(|| {
        std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
            .ok()
            .map(|id| id.trim().to_owned())
    })
    .as_deref()
}

fn digest(bytes: &[u8]) -> String {
    // 160 bits leave room for the descriptive prefix in FerroState's 64-byte slug.
    format!("{:x}", Sha256::digest(bytes))[..40].to_owned()
}

fn stream_slug(stream: &StreamId) -> String {
    format!("{LAUNCH_PREFIX}{}", digest(stream.to_string().as_bytes()))
}

fn generation_slug(session_id: &str, generation: &str) -> String {
    let key = format!("{session_id}/{generation}");
    format!("{GENERATION_PREFIX}{}", digest(key.as_bytes()))
}

fn baseline_slug(receipt: &LaunchReceipt) -> String {
    let key = format!("{}/{}", receipt.host, receipt.vendor);
    format!("{BASELINE_PREFIX}{}", digest(key.as_bytes()))
}

fn queue_slug() -> String {
    format!("{QUEUE_PREFIX}{}", local_host_key())
}

fn read<S: StateStore, T: serde::de::DeserializeOwned>(
    store: &S,
    slug: &str,
) -> Result<Option<T>, LifecycleError> {
    let entry = store.get_state_by_key(&StateScope::Workspace, StateConcern::Runtime, slug)?;
    Ok(entry.and_then(decode))
}

fn decode<T: serde::de::DeserializeOwned>(entry: fl_core::state::StateEntry) -> Option<T> {
    let StatePayload::Runtime(runtime) = entry.payload else {
        return None;
    };
    serde_json::from_value(runtime.value).ok()
}

fn write_args<T: Serialize>(slug: String, value: &T) -> Result<UpsertStateArgs, LifecycleError> {
    Ok(UpsertStateArgs {
        scope: StateScope::Workspace,
        slug,
        sensitivity: SensitivityPatch::SetSensitive,
        payload: StatePayload::Runtime(RuntimeEntry {
            value: serde_json::to_value(value)
                .map_err(|error| LifecycleError::Store(error.to_string()))?,
            merge_mode: MergeMode::LastWriter,
        }),
        at: stamp::now(),
        by: ACTOR.into(),
    })
}

fn write<S: StateStore, T: Serialize>(
    store: &S,
    slug: String,
    value: &T,
) -> Result<(), LifecycleError> {
    store.upsert_state(write_args(slug, value)?)?;
    Ok(())
}

fn load_revision<S: StateStore, T: serde::de::DeserializeOwned + Default>(
    store: &S,
    slug: &str,
) -> Result<(T, Option<RevisionId>), LifecycleError> {
    let Some(entry) =
        store.get_state_by_key(&StateScope::Workspace, StateConcern::Runtime, slug)?
    else {
        return Ok((T::default(), None));
    };
    let revision = entry.head_revision.clone();
    let payload =
        decode(entry).ok_or_else(|| LifecycleError::Store(format!("invalid state for {slug}")))?;
    Ok((payload, Some(revision)))
}

fn update_pending<S: StateStore>(
    store: &S,
    mut edit: impl FnMut(&mut Vec<LaunchReceipt>) -> bool,
) -> Result<bool, LifecycleError> {
    let slug = queue_slug();
    for _ in 0..STATE_UPDATE_ATTEMPTS {
        let (mut pending, revision) = load_revision(store, &slug)?;
        if !edit(&mut pending) {
            return Ok(false);
        }
        if store
            .upsert_state_if_current(write_args(slug.clone(), &pending)?, revision.as_ref())?
            .is_some()
        {
            return Ok(true);
        }
    }
    Err(LifecycleError::Conflict(
        "hosted launch evidence queue is busy".into(),
    ))
}

/// Save the already available launch identity. Adapter probes belong to the worker.
pub(super) fn remember_launch<S: StateStore>(
    store: &S,
    record: &HostedSessionRecord,
    stream: Option<StreamId>,
    leader: Option<ProcessIdentity>,
    config: Option<&CheckedVendorConfig>,
    by: &str,
) {
    let Some(generation) = record.hydration_generation.as_ref() else {
        return;
    };
    if ADAPTERS
        .get()
        .and_then(|adapters| adapters.lookup(&record.vendor))
        .is_none()
    {
        return;
    }
    let id = uuid::Uuid::now_v7();
    let receipt = LaunchReceipt {
        id,
        captured_at_ms: now_ms(),
        host: local_host_key().to_owned(),
        session_id: record.session_id.clone(),
        generation: generation.clone(),
        stream,
        vendor: record.vendor.clone(),
        daemon_expected: config.is_some_and(|config| config.daemon_expected),
        evidence: Some(LaunchEvidence {
            boot_id: boot_id().map(str::to_owned),
            leader,
            daemon: config.and_then(|config| config.daemon.clone()),
            automatic: by == AUTO_RESPAWN_ACTOR,
        }),
        versions: None,
    };
    let queued = update_pending(store, |pending| {
        if pending.len() >= MAX_PENDING_LAUNCHES {
            return false;
        }
        pending.push(receipt.clone());
        true
    });
    if !matches!(queued, Ok(true)) {
        // Keep the exact launch visible as an unknown observation when the queue cannot accept it.
        let versions = HostedVendorVersions {
            daemon_expected: receipt.daemon_expected,
            ..Default::default()
        };
        if let Err(error) = save_versions(store, receipt, versions) {
            eprintln!("hub: error recording hosted launch version evidence: {error}");
        }
    }
}

/// Read version evidence by exact Host instance, session, and epoch.
pub fn hosted_vendor_versions_for_stream<S: StateStore>(
    store: &S,
    stream: &StreamId,
) -> Result<Option<HostedVendorVersions>, LifecycleError> {
    Ok(versions_for_stream(
        read(store, &stream_slug(stream))?,
        stream,
    ))
}

/// Read versions for Host inventory from one local read-only store snapshot.
pub fn read_hosted_vendor_versions(
    path: &Path,
    streams: &[&StreamId],
) -> Result<Vec<Option<HostedVendorVersions>>, LifecycleError> {
    let keys = streams
        .iter()
        .map(|stream| stream_slug(stream))
        .collect::<Vec<_>>();
    let entries = fl_store::health::read_state_keys(
        path,
        None,
        &StateScope::Workspace,
        StateConcern::Runtime,
        &keys,
        0,
    )?;
    Ok(entries
        .into_iter()
        .zip(streams)
        .map(|(entry, stream)| versions_for_stream(entry.and_then(decode), stream))
        .collect())
}

fn versions_for_stream(
    receipt: Option<LaunchReceipt>,
    stream: &StreamId,
) -> Option<HostedVendorVersions> {
    receipt
        .filter(|receipt| receipt.stream.as_ref() == Some(stream))
        .and_then(|receipt| receipt.versions)
}

pub(crate) fn hosted_vendor_versions_for_record<S: StateStore>(
    store: &S,
    record: &HostedSessionRecord,
) -> Option<HostedVendorVersions> {
    let generation = record.hydration_generation.as_ref()?;
    let respawn = load_hosted_respawn_state(store, &record.session_id).ok()?;
    let stream = respawn.exit_evidence.bound_stream(record);
    let slug = stream.map_or_else(
        || generation_slug(&record.session_id, generation),
        stream_slug,
    );
    let receipt: LaunchReceipt = read(store, &slug).ok()??;
    if receipt.generation != *generation
        || receipt.session_id != record.session_id
        || stream.is_some_and(|bound| receipt.stream.as_ref() != Some(bound))
    {
        return None;
    }
    if stream.is_none() {
        // A legacy resume can reuse a generation. Hide older results until its
        // replacement has finished observation, even when it has no native epoch.
        let (pending, _): (Vec<LaunchReceipt>, _) = load_revision(store, &queue_slug()).ok()?;
        if pending.iter().any(|launch| {
            launch.session_id == record.session_id
                && launch.generation == *generation
                && launch.id > receipt.id
        }) {
            return None;
        }
    }
    receipt.versions
}

fn observe(receipt: &LaunchReceipt, adapters: &VendorVersionRegistry) -> HostedVendorVersions {
    let mut versions = HostedVendorVersions {
        daemon_expected: receipt.daemon_expected,
        ..Default::default()
    };
    let Some(evidence) = &receipt.evidence else {
        return versions;
    };
    let Some(adapter) = adapters.lookup(&receipt.vendor) else {
        return versions;
    };
    if evidence
        .boot_id
        .as_deref()
        .zip(boot_id())
        .is_none_or(|(before, now)| before != now)
    {
        return versions;
    }
    versions.cli = evidence
        .leader
        .as_ref()
        .and_then(|leader| leader.cli_version(adapter, 2));
    versions.daemon = evidence
        .daemon
        .as_ref()
        .and_then(|daemon| daemon.observe(adapter, !evidence.automatic));
    versions
}

fn save_versions<S: StateStore>(
    store: &S,
    mut receipt: LaunchReceipt,
    versions: HostedVendorVersions,
) -> Result<(), LifecycleError> {
    receipt.evidence = None;
    receipt.versions = Some(versions);
    let receipt_slug = receipt
        .stream
        .as_ref()
        .map_or_else(|| format!("{LAUNCH_PREFIX}{}", receipt.id), stream_slug);
    write(store, receipt_slug, &receipt)?;
    let slug = generation_slug(&receipt.session_id, &receipt.generation);
    for _ in 0..STATE_UPDATE_ATTEMPTS {
        let current =
            store.get_state_by_key(&StateScope::Workspace, StateConcern::Runtime, &slug)?;
        let revision = current.as_ref().map(|entry| entry.head_revision.clone());
        if current
            .and_then(decode::<LaunchReceipt>)
            .is_some_and(|prior| prior.id >= receipt.id)
        {
            return Ok(());
        }
        if store
            .upsert_state_if_current(write_args(slug.clone(), &receipt)?, revision.as_ref())?
            .is_some()
        {
            return Ok(());
        }
    }
    Err(LifecycleError::Conflict(
        "hosted launch generation index is busy".into(),
    ))
}

fn compare_versions<S: StateStore>(
    store: &S,
    receipt: &LaunchReceipt,
    versions: HostedVendorVersions,
) -> Result<HostedVendorVersions, LifecycleError> {
    let slug = baseline_slug(receipt);
    for _ in 0..STATE_UPDATE_ATTEMPTS {
        let (mut baseline, revision): (VendorBaseline, _) = load_revision(store, &slug)?;
        if let Some((id, versions)) = baseline.last_applied.as_ref()
            && *id == receipt.id
        {
            return Ok(versions.clone());
        }
        let compared = baseline.apply(receipt.id, versions.clone());
        if store
            .upsert_state_if_current(write_args(slug.clone(), &baseline)?, revision.as_ref())?
            .is_some()
        {
            return Ok(compared);
        }
    }
    Err(LifecycleError::Conflict(
        "hosted launch version baseline is busy".into(),
    ))
}

fn commit<S: StateStore>(
    store: &S,
    receipt: LaunchReceipt,
    versions: HostedVendorVersions,
) -> Result<Option<HostedVendorVersions>, LifecycleError> {
    let (pending, _): (Vec<LaunchReceipt>, _) = load_revision(store, &queue_slug())?;
    if !pending.iter().any(|current| current.id == receipt.id) {
        return Ok(None);
    }
    let versions = compare_versions(store, &receipt, versions)?;
    let id = receipt.id;
    save_versions(store, receipt, versions.clone())?;
    let removed = update_pending(store, |pending| {
        let count = pending.len();
        pending.retain(|current| current.id != id);
        pending.len() < count
    })?;
    Ok(removed.then_some(versions))
}

pub(crate) fn observe_launches_in_hub(state: &crate::HubState) -> Result<(), LifecycleError> {
    let Some(adapters) = ADAPTERS.get() else {
        return Ok(());
    };
    let batch = {
        let store = state.try_open_version_observation_store()?;
        let now = now_ms();
        let (mut pending, _): (Vec<LaunchReceipt>, _) = load_revision(&*store, &queue_slug())?;
        pending.sort_unstable_by_key(|receipt| receipt.id);
        pending
            .into_iter()
            .filter(|receipt| now.saturating_sub(receipt.captured_at_ms) >= SETTLE_MS)
            .take(OBSERVATIONS_PER_PASS)
            .collect::<Vec<_>>()
    };
    for receipt in batch {
        // All process and daemon reads run outside the workspace store guard and persona fence.
        let versions = observe(&receipt, adapters);
        let session_id = receipt.session_id.clone();
        let stream = receipt.stream.clone();
        let generation = receipt.generation.clone();
        let vendor = receipt.vendor.clone();
        let applied = {
            let store = state.try_open_version_observation_store()?;
            commit(&*store, receipt, versions)?
        };
        if let Some(versions) = applied {
            for change in &versions.changes {
                eprintln!(
                    "hub: warning ({session_id}): {vendor} {:?} version changed from {} to {}",
                    change.component, change.previous, change.current,
                );
            }
            let event = if versions.changes.is_empty() {
                "session.vendor_versions_observed"
            } else {
                "session.vendor_version_changed"
            };
            state.broadcast(
                "sessions",
                event,
                serde_json::json!({
                    "session_id": session_id,
                    "hydration_generation": generation,
                    "stream": stream,
                    "vendor_versions": versions,
                }),
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use fl_core::MemStore;
    use fl_host_proto::{HostInstance, SessionEpoch, SessionId};

    fn sample(cli: Option<&str>, daemon: Option<&str>) -> HostedVendorVersions {
        HostedVendorVersions {
            cli: cli.map(str::to_owned),
            daemon: daemon.map(str::to_owned),
            daemon_expected: true,
            changes: Vec::new(),
        }
    }

    fn launch(index: u8) -> uuid::Uuid {
        uuid::Uuid::from_u128(u128::from(index))
    }

    fn receipt(index: u8) -> LaunchReceipt {
        LaunchReceipt {
            id: launch(index),
            captured_at_ms: now_ms(),
            host: local_host_key().into(),
            session_id: "codex:ada".into(),
            generation: next_hydration_generation(),
            stream: Some(StreamId {
                host: HostInstance::new([1; 16]),
                session: SessionId::new("Ada.codex").unwrap(),
                epoch: SessionEpoch(u64::from(index)),
            }),
            vendor: "codex".into(),
            daemon_expected: true,
            evidence: Some(LaunchEvidence {
                boot_id: boot_id().map(str::to_owned),
                leader: None,
                daemon: None,
                automatic: false,
            }),
            versions: None,
        }
    }

    fn seed(store: &MemStore, receipt: &LaunchReceipt) {
        assert!(
            update_pending(store, |pending| {
                pending.push(receipt.clone());
                true
            })
            .unwrap()
        );
    }

    #[test]
    fn versions_compare_last_known_components_independently() {
        let mut baseline = VendorBaseline::default();
        for (index, cli, daemon) in [
            (1, Some("1.0.0"), Some("2.0.0")),
            (2, None, Some("2.0.0")),
            (3, Some("1.0.0"), None),
            (4, None, None),
        ] {
            assert!(
                baseline
                    .apply(launch(index), sample(cli, daemon))
                    .changes
                    .is_empty()
            );
        }
        let observed = baseline.apply(launch(6), sample(Some("1.1.0"), None));
        assert_eq!(
            observed.changes,
            vec![HostedVendorVersionChange {
                component: HostedVendorVersionComponent::Cli,
                previous: "1.0.0".into(),
                current: "1.1.0".into(),
            }]
        );
        let delayed = baseline.apply(launch(5), sample(Some("0.9.0"), Some("2.1.0")));
        assert_eq!(
            delayed.changes,
            vec![HostedVendorVersionChange {
                component: HostedVendorVersionComponent::Daemon,
                previous: "2.0.0".into(),
                current: "2.1.0".into(),
            }]
        );
        assert!(
            baseline
                .apply(launch(7), sample(Some("1.1.0"), Some("2.1.0")))
                .changes
                .is_empty()
        );
    }

    #[test]
    fn versions_commit_is_exact_and_replay_safe() {
        let store = MemStore::default();
        let first = receipt(1);
        seed(&store, &first);
        let second = receipt(2);
        seed(&store, &second);
        let first_stream = first.stream.clone().unwrap();
        let second_stream = second.stream.clone().unwrap();
        assert!(
            commit(&store, first, sample(Some("1.0.0"), None))
                .unwrap()
                .unwrap()
                .changes
                .is_empty()
        );
        let observed = commit(&store, second.clone(), sample(Some("1.1.0"), Some("2.0.0")))
            .unwrap()
            .unwrap();
        assert_eq!(observed.changes.len(), 1);
        assert!(
            commit(&store, second, sample(Some("3.0.0"), None))
                .unwrap()
                .is_none()
        );
        assert_eq!(
            hosted_vendor_versions_for_stream(&store, &first_stream).unwrap(),
            Some(sample(Some("1.0.0"), None))
        );
        assert_eq!(
            hosted_vendor_versions_for_stream(&store, &second_stream).unwrap(),
            Some(observed)
        );
        let mut replacement = second_stream;
        replacement.host = HostInstance::new([2; 16]);
        assert!(
            hosted_vendor_versions_for_stream(&store, &replacement)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn host_inventory_reads_exact_streams_from_a_local_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.fls");
        let store = fl_store::FerroStore::open(&path).unwrap();
        let receipt = receipt(1);
        let stream = receipt.stream.clone().unwrap();
        let mut unmatched = stream.clone();
        unmatched.host = HostInstance::new([2; 16]);
        let observed = sample(Some("1.0.0"), None);
        save_versions(&store, receipt, observed.clone()).unwrap();

        assert_eq!(
            read_hosted_vendor_versions(&path, &[&unmatched, &stream]).unwrap(),
            vec![None, Some(observed)]
        );
    }

    #[test]
    fn versions_restart_retains_warning_after_baseline_checkpoint() {
        let store = MemStore::default();
        let first = receipt(1);
        let second = receipt(2);
        seed(&store, &second);
        let baseline_slug = baseline_slug(&second);
        let mut baseline = VendorBaseline::default();
        baseline.apply(first.id, sample(Some("1.0.0"), None));
        let expected = baseline.apply(second.id, sample(Some("2.0.0"), None));
        write(&store, baseline_slug, &baseline).unwrap();
        // The process can disappear after the checkpoint and before saving the receipt.
        let recovered = commit(&store, second, sample(None, None)).unwrap();
        assert_eq!(recovered, Some(expected));
    }

    #[test]
    fn versions_keep_distinct_legacy_launches_that_reuse_a_generation() {
        let store = MemStore::default();
        let mut first = receipt(1);
        let mut second = receipt(2);
        first.stream = None;
        second.stream = None;
        second.generation.clone_from(&first.generation);
        let generation = generation_slug(&second.session_id, &second.generation);
        let first_slug = format!("{LAUNCH_PREFIX}{}", first.id);
        let second_id = second.id;
        let old = sample(Some("1.0.0"), None);
        let new = sample(Some("2.0.0"), None);
        // A delayed earlier result must not replace the current generation projection.
        save_versions(&store, second, new.clone()).unwrap();
        save_versions(&store, first, old.clone()).unwrap();
        let earlier: LaunchReceipt = read(&store, &first_slug).unwrap().unwrap();
        let current: LaunchReceipt = read(&store, &generation).unwrap().unwrap();
        assert_eq!(earlier.versions, Some(old));
        assert_eq!(current.id, second_id);
        assert_eq!(current.versions, Some(new));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn versions_never_read_an_executable_for_a_reused_pid() {
        let mut process = ProcessIdentity::capture(std::process::id()).unwrap();
        assert!(process.executable().is_some());
        process.start_ticks += 1;
        assert!(process.executable().is_none());
    }
}
