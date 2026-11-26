//! Disjoint hosted launch receipts and host-local vendor version baselines.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use fl_core::state::{
    HOSTED_VERSION_BASELINE_PREFIX, HOSTED_VERSION_GENERATION_PREFIX, HOSTED_VERSION_LAUNCH_PREFIX,
    HOSTED_VERSION_QUEUE_PREFIX,
};
use fl_core::vendor_adapter::VendorDaemonLaunchEvidence;
use fl_core::{RevisionId, VendorVersionRegistry};
use fl_host_proto::StreamId;
use fl_store::{AnyStore, BackendKind, StoreOptions, Unmigrated, choose_backend, physical_path};
use sha2::{Digest, Sha256};

use super::*;

const ACTOR: &str = "hub-vendor-versions";
const OBSERVATIONS_PER_PASS: usize = 8;
const MAX_PENDING_LAUNCHES: usize = 64;
const STATE_UPDATE_ATTEMPTS: usize = 4;
const SETTLE_MS: u64 = 250;
const MAX_WRAPPER_CHILDREN: usize = 8;
const DIRECT_COMMIT_WAIT: Duration = Duration::from_millis(100);

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

impl HostedVendorVersions {
    fn fill_missing(&mut self, observed: Self) {
        self.cli = self.cli.take().or(observed.cli);
        self.daemon = self.daemon.take().or(observed.daemon);
    }

    fn complete(&self) -> bool {
        self.cli.is_some() && (!self.daemon_expected || self.daemon.is_some())
    }
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

    #[cfg(target_os = "linux")]
    fn manual_cli_version(&self, adapter: &dyn fl_core::VendorVersionAdapter) -> Option<String> {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::{FileExt, MetadataExt};
        use std::os::unix::process::CommandExt;
        use std::process::Command;
        use std::time::Duration;

        const TIMEOUT: Duration = Duration::from_millis(500);

        let executable = self.executable()?;
        let probe = adapter.manual_cli_version_probe(&executable)?;
        let process_image = PathBuf::from(format!("/proc/{}/exe", self.pid));
        let image = std::fs::File::open(&process_image).ok()?;
        // Keep the observed inode open through exec even if the installer replaces its pathname.
        let pinned = PathBuf::from(format!(
            "/proc/{}/fd/{}",
            std::process::id(),
            image.as_raw_fd()
        ));
        let metadata = image.metadata().ok()?;
        let current = std::fs::metadata(process_image).ok()?;
        let mut magic = [0; 4];
        if !metadata.is_file()
            || metadata.mode() & 0o6000 != 0
            || (metadata.dev(), metadata.ino()) != (current.dev(), current.ino())
            || std::fs::read_link(&pinned).ok()? != executable
            || !self.is_current()
            || image.read_exact_at(&mut magic, 0).is_err()
            || &magic != b"\x7fELF"
        {
            return None;
        }

        let home = tempfile::tempdir().ok()?;
        let mut command = Command::new(pinned);
        command
            .arg0(executable)
            .env_clear()
            .env("HOME", home.path())
            .env("TMPDIR", home.path())
            .env("PATH", "")
            .envs(probe.env.iter().copied())
            .current_dir(home.path());
        let output = crate::runtime_matrix::run_version_command(command, TIMEOUT).ok()?;
        (probe.parse)(&output).map(str::to_owned)
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
    #[serde(default)]
    direct: bool,
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
        if let Some((applied, previous)) = &self.last_applied
            && *applied == launch
        {
            let mut recovered = previous.clone();
            recovered.fill_missing(versions);
            versions = recovered;
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
    format!(
        "{HOSTED_VERSION_LAUNCH_PREFIX}{}",
        digest(stream.to_string().as_bytes())
    )
}

fn generation_slug(session_id: &str, generation: &str) -> String {
    let key = format!("{session_id}/{generation}");
    format!(
        "{HOSTED_VERSION_GENERATION_PREFIX}{}",
        digest(key.as_bytes())
    )
}

fn baseline_slug(receipt: &LaunchReceipt) -> String {
    let key = format!("{}/{}", receipt.host, receipt.vendor);
    format!("{HOSTED_VERSION_BASELINE_PREFIX}{}", digest(key.as_bytes()))
}

fn queue_slug() -> String {
    format!("{HOSTED_VERSION_QUEUE_PREFIX}{}", local_host_key())
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

fn queue_launch<S: StateStore>(store: &S, receipt: &LaunchReceipt) -> Result<bool, LifecycleError> {
    update_pending(store, |pending| {
        if pending.len() >= MAX_PENDING_LAUNCHES {
            // A direct CLI can accumulate entries already recorded in its store that
            // are only waiting for Hub notification or enrichment. Reclaim one before
            // giving up any launch that still has no observation.
            let Some(index) = pending
                .iter()
                .position(|receipt| receipt.versions.is_some())
            else {
                return false;
            };
            pending.remove(index);
        }
        pending.push(receipt.clone());
        true
    })
}

/// Save the already available launch identity. Adapter probes belong to the worker.
pub(super) fn remember_launch<S: StateStore>(
    store: &S,
    env: &LifecycleEnv<'_>,
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
            direct: env.direct_version_receipts.is_some(),
        }),
        versions: None,
    };
    let queued = queue_launch(store, &receipt);
    if let Some(receipts) = env.direct_version_receipts {
        // Even a saturated queue needs a direct pass to recover older skipped receipts.
        receipts.lock().unwrap_or_else(|e| e.into_inner()).push(id);
    }
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

fn observe(
    receipt: &LaunchReceipt,
    adapters: &VendorVersionRegistry,
    allow_manual_probe: bool,
) -> HostedVendorVersions {
    let mut versions = receipt
        .versions
        .clone()
        .unwrap_or_else(|| HostedVendorVersions {
            daemon_expected: receipt.daemon_expected,
            ..Default::default()
        });
    if receipt.host != local_host_key() {
        return versions;
    }
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
    let allow_manual_probe = allow_manual_probe && !evidence.automatic;
    versions.cli = versions.cli.or_else(|| {
        evidence.leader.as_ref().and_then(|leader| {
            let passive = leader.cli_version(adapter, 2);
            #[cfg(target_os = "linux")]
            if passive.is_none() && allow_manual_probe {
                return leader.manual_cli_version(adapter);
            }
            passive
        })
    });
    versions.daemon = versions.daemon.or_else(|| {
        evidence
            .daemon
            .as_ref()
            .and_then(|daemon| daemon.observe(adapter, allow_manual_probe))
    });
    versions
}

fn save_versions<S: StateStore>(
    store: &S,
    mut receipt: LaunchReceipt,
    versions: HostedVendorVersions,
) -> Result<(), LifecycleError> {
    receipt.evidence = None;
    receipt.versions = Some(versions);
    let receipt_slug = receipt.stream.as_ref().map_or_else(
        || format!("{HOSTED_VERSION_LAUNCH_PREFIX}{}", receipt.id),
        stream_slug,
    );
    write(store, receipt_slug, &receipt)?;
    let slug = generation_slug(&receipt.session_id, &receipt.generation);
    for _ in 0..STATE_UPDATE_ATTEMPTS {
        let current =
            store.get_state_by_key(&StateScope::Workspace, StateConcern::Runtime, &slug)?;
        let revision = current.as_ref().map(|entry| entry.head_revision.clone());
        if current
            .and_then(decode::<LaunchReceipt>)
            .is_some_and(|prior| {
                prior.id > receipt.id
                    || (prior.id == receipt.id && prior.versions == receipt.versions)
            })
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

#[derive(Debug, PartialEq, Eq)]
struct CommittedObservation {
    versions: HostedVendorVersions,
    already_reported: usize,
}

impl CommittedObservation {
    fn warn_about_drift(&self, prefix: &str, session_id: &str, vendor: &str) {
        for change in self.versions.changes.iter().skip(self.already_reported) {
            eprintln!(
                "{prefix}warning ({session_id}): {vendor} {:?} version changed from {} to {}",
                change.component, change.previous, change.current,
            );
        }
    }
}

fn commit<S: StateStore>(
    store: &S,
    receipt: LaunchReceipt,
    observed: HostedVendorVersions,
    finalize: bool,
) -> Result<Option<CommittedObservation>, LifecycleError> {
    let (pending, _): (Vec<LaunchReceipt>, _) = load_revision(store, &queue_slug())?;
    let Some(current) = pending.into_iter().find(|current| current.id == receipt.id) else {
        return Ok(None);
    };
    let mut versions = current.versions.unwrap_or(HostedVendorVersions {
        daemon_expected: current.daemon_expected,
        ..Default::default()
    });
    let already_reported = versions.changes.len();
    versions.fill_missing(observed);
    let versions = compare_versions(store, &receipt, versions)?;
    let id = receipt.id;
    save_versions(store, receipt, versions.clone())?;
    let applied = update_pending(store, |pending| {
        let Some(index) = pending.iter().position(|current| current.id == id) else {
            return false;
        };
        if finalize {
            pending.remove(index);
        } else {
            pending[index].versions = Some(versions.clone());
            // Keep the durable queue item so a Hub owning this store can announce
            // the observation, and enrich any still-missing component.
            if versions.complete() {
                pending[index].evidence = None;
            }
        }
        true
    })?;
    Ok(applied.then_some(CommittedObservation {
        versions,
        already_reported,
    }))
}

pub(super) fn observe_direct_launches<S: StateStore>(
    store: &S,
    ferro_file: Option<&Path>,
    receipts: &[uuid::Uuid],
) -> Result<(), LifecycleError> {
    let Some(adapters) = ADAPTERS.get() else {
        return Ok(());
    };
    observe_direct_with_adapters(store, ferro_file, receipts, adapters)
}

fn observe_direct_with_adapters<S: StateStore>(
    store: &S,
    ferro_file: Option<&Path>,
    receipts: &[uuid::Uuid],
    adapters: &VendorVersionRegistry,
) -> Result<(), LifecycleError> {
    let Some(latest) = receipts.iter().max() else {
        return Ok(());
    };
    let file = ferro_file
        .map(Path::canonicalize)
        .transpose()
        .map_err(|err| LifecycleError::Store(err.to_string()))?;
    let (mut pending, _): (Vec<LaunchReceipt>, _) = load_revision(store, &queue_slug())?;
    pending.sort_unstable_by_key(|receipt| receipt.id);
    // Recover older direct receipts whose CLI exited before it could settle. When both
    // launches are already queued, settle the older one first for drift comparison.
    let mut recover = OBSERVATIONS_PER_PASS;
    let sampled: Vec<_> = pending
        .into_iter()
        .filter(|receipt| {
            if receipts.contains(&receipt.id) {
                return true;
            }
            if recover > 0
                && receipt.id < *latest
                && receipt.versions.is_none()
                && receipt
                    .evidence
                    .as_ref()
                    .is_some_and(|evidence| evidence.direct)
            {
                recover -= 1;
                return true;
            }
            false
        })
        .map(|receipt| {
            let versions = observe(&receipt, adapters, false);
            (receipt, versions)
        })
        .collect();
    let started = Instant::now();
    for (receipt, versions) in sampled {
        let session_id = receipt.session_id.clone();
        let vendor = receipt.vendor.clone();
        let applied = {
            let _commit = if let Some(file) = &file {
                let wait = DIRECT_COMMIT_WAIT.saturating_sub(started.elapsed());
                let Some(guard) = lock::try_version_commit(file, wait)? else {
                    // A subsequent direct invocation or the Hub can settle the queued receipt.
                    return Ok(());
                };
                Some(guard)
            } else {
                // The direct CLI's redb handle already owns the file exclusively.
                None
            };
            commit(store, receipt, versions, false)?
        };
        if let Some(applied) = applied {
            applied.warn_about_drift("", &session_id, &vendor);
        }
    }
    Ok(())
}

/// The file that owns the queue now. Re-resolve it before each commit: a migration or a
/// changed symlink can switch the queue to a file whose observer holds a different lock.
#[derive(PartialEq, Eq)]
struct ObservationTarget {
    file: PathBuf,
    selection: BackendKind,
    engine: BackendKind,
}

impl ObservationTarget {
    fn resolve(state: &crate::HubState) -> Result<Option<Self>, LifecycleError> {
        let logical = &state.config.store_path;
        let selection = match state.store_backend {
            Some(engine) => engine,
            None => choose_backend(
                logical,
                std::env::var(fl_store::BACKEND_ENV).ok().as_deref(),
            )?,
        };
        let physical = physical_path(logical, selection);
        let (file, engine) = match std::fs::symlink_metadata(&physical) {
            Ok(_) => (physical, selection),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if selection != BackendKind::Ferro || physical == *logical || !logical.is_file() {
                    return Ok(None);
                }
                (logical.to_path_buf(), BackendKind::Redb)
            }
            Err(error) => return Err(LifecycleError::Store(error.to_string())),
        };
        let file = file
            .canonicalize()
            .map_err(|error| LifecycleError::Store(error.to_string()))?;
        Ok(file.is_file().then_some(Self {
            file,
            selection,
            engine,
        }))
    }

    fn is_current(&self, state: &crate::HubState) -> Result<bool, LifecycleError> {
        Ok(Self::resolve(state)?.as_ref() == Some(self))
    }
}

/// Owns only the multiprocess-safe Ferro handle. A redb open lasts one short operation.
#[derive(Default)]
pub(crate) struct VersionObserver {
    ferro: Option<(PathBuf, AnyStore)>,
}

impl VersionObserver {
    pub(crate) fn observe_launches(
        &mut self,
        state: &crate::HubState,
    ) -> Result<(), LifecycleError> {
        if let Some(adapters) = ADAPTERS.get() {
            self.observe_with_adapters(state, adapters)?;
        }
        Ok(())
    }

    fn with_store<T>(
        &mut self,
        state: &crate::HubState,
        target: &ObservationTarget,
        operation: impl FnOnce(&AnyStore) -> Result<T, LifecycleError>,
    ) -> Result<T, LifecycleError> {
        // No Hub store guard is held during a Ferro open or use. On redb, the engine's
        // own nonblocking file lock excludes migration and other writers.
        let open = || {
            let store = fl_store::open_store_with(
                &state.config.store_path,
                &StoreOptions {
                    backend: Some(target.selection),
                    retries: 0,
                    unmigrated: Unmigrated::WriteRedb,
                },
            )?;
            if store.kind() != target.engine || !target.is_current(state)? {
                return Err(LifecycleError::Conflict(
                    "version observer store changed".into(),
                ));
            }
            Ok(store)
        };
        if target.engine == BackendKind::Redb {
            self.ferro = None;
            return operation(&open()?);
        }
        if self
            .ferro
            .as_ref()
            .is_none_or(|(path, _)| path != &target.file)
        {
            self.ferro = Some((target.file.clone(), open()?));
        }
        let (_, store) = self.ferro.as_ref().expect("Ferro handle was opened");
        let result = operation(store);
        // Reopen on the next pass: an error may leave a local handle poisoned or a
        // served handle disconnected.
        if result.is_err() {
            self.ferro = None;
        }
        result
    }

    fn observe_with_adapters(
        &mut self,
        state: &crate::HubState,
        adapters: &VendorVersionRegistry,
    ) -> Result<(), LifecycleError> {
        let Some(target) = ObservationTarget::resolve(state)? else {
            self.ferro = None;
            return Ok(());
        };
        let _worker = match target.engine {
            BackendKind::Ferro => {
                let Some(worker) = lock::try_version_observer(&target.file)? else {
                    return Ok(());
                };
                Some(worker)
            }
            // Redb exclusively owns the file for the whole commit, including the pending
            // recheck. Competing observers may probe twice but only one can settle a launch.
            BackendKind::Redb => None,
        };
        if !target.is_current(state)? {
            return Ok(());
        }
        let batch = self.with_store(state, &target, |store| {
            let now = now_ms();
            let (mut pending, _): (Vec<LaunchReceipt>, _) = load_revision(store, &queue_slug())?;
            pending.sort_unstable_by_key(|receipt| receipt.id);
            Ok(pending
                .into_iter()
                .filter(|receipt| now.saturating_sub(receipt.captured_at_ms) >= SETTLE_MS)
                .take(OBSERVATIONS_PER_PASS)
                .collect::<Vec<_>>())
        })?;
        for receipt in batch {
            // Only Hub sampling uses the observer lock. Process and daemon reads hold
            // no commit lock, workspace transaction, redb handle or persona fence.
            let versions = observe(&receipt, adapters, true);
            let session_id = receipt.session_id.clone();
            let stream = receipt.stream.clone();
            let generation = receipt.generation.clone();
            let vendor = receipt.vendor.clone();
            let applied = {
                let _commit = if target.engine == BackendKind::Ferro {
                    let Some(guard) = lock::try_version_commit(&target.file, Duration::ZERO)?
                    else {
                        return Ok(());
                    };
                    Some(guard)
                } else {
                    None
                };
                if !target.is_current(state)? {
                    // A different file means a different worker lock. Let that worker settle the
                    // persisted queue instead of applying an old observation under this lock.
                    return Ok(());
                }
                self.with_store(state, &target, |store| {
                    commit(store, receipt, versions, true)
                })?
            };
            if let Some(applied) = applied {
                applied.warn_about_drift("hub: ", &session_id, &vendor);
                let versions = applied.versions;
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
                direct: false,
            }),
            versions: None,
        }
    }

    fn seed(store: &impl StateStore, receipt: &LaunchReceipt) {
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
            commit(&store, first, sample(Some("1.0.0"), None), true)
                .unwrap()
                .unwrap()
                .versions
                .changes
                .is_empty()
        );
        let observed = commit(
            &store,
            second.clone(),
            sample(Some("1.1.0"), Some("2.0.0")),
            true,
        )
        .unwrap()
        .unwrap()
        .versions;
        assert_eq!(observed.changes.len(), 1);
        assert!(
            commit(&store, second, sample(Some("3.0.0"), None), true)
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
    fn versions_keep_partial_direct_drift_when_worker_adds_daemon() {
        let store = MemStore::default();
        let first = receipt(1);
        let mut second = receipt(2);
        second.stream = None;
        seed(&store, &first);
        seed(&store, &second);
        commit(&store, first, sample(Some("1.0.0"), Some("2.0.0")), true).unwrap();

        let direct = commit(&store, second.clone(), sample(Some("1.1.0"), None), false)
            .unwrap()
            .unwrap();
        assert_eq!(direct.already_reported, 0);
        assert_eq!(direct.versions.changes.len(), 1);
        assert_eq!(
            direct.versions.changes[0].component,
            HostedVendorVersionComponent::Cli
        );

        let slug = generation_slug(&second.session_id, &second.generation);
        let partial: LaunchReceipt = read(&store, &slug).unwrap().unwrap();
        assert_eq!(partial.versions, Some(direct.versions));

        // A worker that already sampled its batch can complete this observation using a
        // stale receipt. The previously observed CLI and its warning remain authoritative.
        let final_observation = commit(&store, second, sample(Some("9.9.9"), Some("2.1.0")), true)
            .unwrap()
            .unwrap();
        assert_eq!(final_observation.already_reported, 1);
        assert_eq!(final_observation.versions.cli.as_deref(), Some("1.1.0"));
        assert_eq!(final_observation.versions.daemon.as_deref(), Some("2.1.0"));
        assert_eq!(final_observation.versions.changes.len(), 2);
        assert_eq!(
            final_observation.versions.changes[1].component,
            HostedVendorVersionComponent::Daemon
        );
        let saved: LaunchReceipt = read(&store, &slug).unwrap().unwrap();
        assert_eq!(saved.versions, Some(final_observation.versions));
    }

    #[test]
    fn versions_at_capacity_recover_and_reclaim_direct_observations() {
        let store = MemStore::default();
        let unresolved = receipt(1);
        let mut recoverable = receipt(2);
        recoverable.evidence.as_mut().unwrap().direct = true;
        let limit = u8::try_from(MAX_PENDING_LAUNCHES).unwrap();
        seed(&store, &unresolved);
        seed(&store, &recoverable);
        for index in 3..=limit {
            seed(&store, &receipt(index));
        }
        let next = receipt(limit + 1);
        assert!(!queue_launch(&store, &next).unwrap());

        let adapters = VendorVersionRegistry::default();
        observe_direct_with_adapters(&store, None, &[next.id], &adapters).unwrap();
        assert!(queue_launch(&store, &next).unwrap());
        let (pending, _): (Vec<LaunchReceipt>, _) = load_revision(&store, &queue_slug()).unwrap();
        assert_eq!(pending.len(), MAX_PENDING_LAUNCHES);
        assert!(pending.iter().any(|item| item.id == unresolved.id));
        assert!(pending.iter().any(|item| item.id == next.id));
        assert!(!pending.iter().any(|item| item.id == recoverable.id));
        assert_eq!(
            hosted_vendor_versions_for_stream(&store, &recoverable.stream.unwrap()).unwrap(),
            Some(sample(None, None)),
        );
    }

    #[test]
    fn versions_observer_announces_a_complete_direct_observation_once() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = crate::HubState::new(crate::HubServerConfig {
            store_path: dir.path().join("workspace.fls"),
            home_override: Some(dir.path().into()),
            ..crate::HubServerConfig::default()
        });
        state.store_backend = Some(BackendKind::Ferro);
        let first = receipt(1);
        let mut second = receipt(2);
        second.captured_at_ms = now_ms().saturating_sub(SETTLE_MS);
        {
            let store = state.open_store().unwrap();
            seed(&*store, &first);
            seed(&*store, &second);
            commit(&*store, first, sample(Some("1.0.0"), Some("2.0.0")), true).unwrap();
            commit(&*store, second, sample(Some("1.1.0"), Some("2.0.0")), false).unwrap();
            let (pending, _): (Vec<LaunchReceipt>, _) =
                load_revision(&*store, &queue_slug()).unwrap();
            assert_eq!(pending.len(), 1);
            assert!(pending[0].evidence.is_none());
        }
        let mut events = state.subscribe();
        let mut observer = VersionObserver::default();
        let adapters = VendorVersionRegistry::default();
        observer.observe_with_adapters(&state, &adapters).unwrap();
        assert_eq!(
            events.try_recv().unwrap().event,
            "session.vendor_version_changed"
        );
        observer.observe_with_adapters(&state, &adapters).unwrap();
        assert!(events.try_recv().is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn versions_competing_direct_observers_settle_without_the_hub_sampler_lock() {
        use std::sync::mpsc;

        #[derive(Debug)]
        struct DirectAdapter {
            observer: u8,
            sampled: mpsc::Sender<u8>,
        }

        impl fl_core::VendorVersionAdapter for DirectAdapter {
            fn vendor_id(&self) -> &'static str {
                "codex"
            }

            fn version_from_executable(&self, executable: &Path) -> Option<String> {
                assert_eq!(executable, std::env::current_exe().unwrap());
                self.sampled.send(self.observer).unwrap();
                Some("2.0.0".into())
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.fls");
        let options = StoreOptions {
            backend: Some(BackendKind::Ferro),
            ..StoreOptions::default()
        };
        let writer = fl_store::open_store_with(&path, &options).unwrap();
        let first_store = fl_store::open_store_with(&path, &options).unwrap();
        let second_store = fl_store::open_store_with(&path, &options).unwrap();
        let path = path.canonicalize().unwrap();

        let before = receipt(1);
        seed(&writer, &before);
        commit(&writer, before, sample(Some("1.0.0"), None), true).unwrap();
        let mut abandoned = receipt(2);
        let mut first = receipt(3);
        let mut second = receipt(4);
        second.session_id = "codex:bea".into();
        second.stream.as_mut().unwrap().session = SessionId::new("Bea.codex").unwrap();
        for receipt in [&mut abandoned, &mut first, &mut second] {
            let evidence = receipt.evidence.as_mut().unwrap();
            evidence.direct = true;
            evidence.leader = ProcessIdentity::capture(std::process::id());
            seed(&writer, receipt);
        }
        let _sampler = lock::try_version_observer(&path).unwrap().unwrap();
        let commit_guard = lock::try_version_commit(&path, Duration::ZERO)
            .unwrap()
            .unwrap();
        let (sampled, on_sampled) = mpsc::channel();
        let observe = |store: &AnyStore, receipt: &LaunchReceipt, observer| {
            let mut adapters = VendorVersionRegistry::default();
            adapters.register(DirectAdapter {
                observer,
                sampled: sampled.clone(),
            });
            observe_direct_with_adapters(store, Some(&path), &[receipt.id], &adapters).unwrap();
        };
        std::thread::scope(|threads| {
            let left = threads.spawn(|| observe(&first_store, &first, 1));
            let right = threads.spawn(|| observe(&second_store, &second, 2));
            // Both CLI observers sample while the Hub sampler and the commit lock are held.
            // The older abandoned receipt is also sampled on the next direct invocation.
            let mut observed = Vec::new();
            for _ in 0..5 {
                observed.push(on_sampled.recv_timeout(Duration::from_secs(2)).unwrap());
            }
            observed.sort_unstable();
            assert_eq!(observed, [1, 1, 2, 2, 2]);
            drop(commit_guard);
            left.join().unwrap();
            right.join().unwrap();
        });

        let versions = [abandoned, first, second].map(|receipt| {
            hosted_vendor_versions_for_stream(&writer, &receipt.stream.unwrap())
                .unwrap()
                .unwrap()
        });
        assert!(versions.iter().all(|v| v.cli.as_deref() == Some("2.0.0")));
        assert_eq!(versions[0].changes.len(), 1);
        assert_eq!(versions[0].changes[0].previous, "1.0.0");
        assert!(versions[1].changes.is_empty());
        assert!(versions[2].changes.is_empty());
        let (baseline, _): (VendorBaseline, _) =
            load_revision(&writer, &baseline_slug(&receipt(4))).unwrap();
        assert_eq!(baseline.cli.unwrap().launch, launch(4));
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
        let recovered = commit(&store, second, sample(None, None), true).unwrap();
        assert_eq!(
            recovered.map(|observation| observation.versions),
            Some(expected)
        );
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
        let first_slug = format!("{HOSTED_VERSION_LAUNCH_PREFIX}{}", first.id);
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

    #[test]
    fn versions_observer_reopens_a_local_store_after_an_operation_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = crate::HubState::new(crate::HubServerConfig {
            store_path: dir.path().join("workspace.fls"),
            home_override: Some(dir.path().into()),
            ..crate::HubServerConfig::default()
        });
        state.store_backend = Some(BackendKind::Ferro);
        drop(state.open_store().unwrap());
        let target = ObservationTarget::resolve(&state).unwrap().unwrap();
        let mut observer = VersionObserver::default();

        let failure = observer.with_store(&state, &target, |store| {
            assert!(!store.is_remote());
            Err::<(), _>(LifecycleError::Store("injected operation failure".into()))
        });
        assert!(failure.is_err());
        assert!(observer.ferro.is_none());

        observer
            .with_store(&state, &target, |store| {
                let launch = receipt(1);
                seed(store, &launch);
                let settled = commit(store, launch, sample(Some("1.0.0"), None), true)?;
                assert!(settled.is_some());
                Ok(())
            })
            .unwrap();
        assert!(observer.ferro.is_some());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn versions_never_read_an_executable_for_a_reused_pid() {
        let mut process = ProcessIdentity::capture(std::process::id()).unwrap();
        assert!(process.executable().is_some());
        process.start_ticks += 1;
        assert!(process.executable().is_none());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn versions_probe_a_manual_hosted_image_and_skip_automatic_launches() {
        use fl_core::vendor_adapter::VendorCliVersionProbe;
        use std::os::unix::fs::symlink;
        use std::process::Command;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        #[derive(Debug)]
        struct FixtureAdapter(Arc<AtomicUsize>);

        impl fl_core::VendorVersionAdapter for FixtureAdapter {
            fn vendor_id(&self) -> &'static str {
                "fixture"
            }

            fn version_from_executable(&self, _executable: &Path) -> Option<String> {
                None
            }

            fn manual_cli_version_probe(&self, executable: &Path) -> Option<VendorCliVersionProbe> {
                assert_eq!(executable.file_name()?, "sleep");
                self.0.fetch_add(1, Ordering::Relaxed);
                Some(VendorCliVersionProbe {
                    env: &[],
                    parse: |line| line.starts_with("sleep (").then_some("1.2.3"),
                })
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let executable = dir.path().join("sleep");
        let installed = dir.path().join("bin/sleep");
        std::fs::create_dir(dir.path().join("bin")).unwrap();
        std::fs::copy("/bin/sleep", &executable).unwrap();
        symlink(&executable, &installed).unwrap();
        let mut child = Command::new(&installed).arg("10").spawn().unwrap();
        let process = ProcessIdentity::capture(child.id()).unwrap();
        std::fs::remove_file(&installed).unwrap();
        symlink("/bin/true", &installed).unwrap();

        let probes = Arc::new(AtomicUsize::new(0));
        let mut adapters = VendorVersionRegistry::default();
        adapters.register(FixtureAdapter(probes.clone()));
        let mut launch = receipt(1);
        launch.vendor = "fixture".into();
        let evidence = launch.evidence.as_mut().unwrap();
        evidence.leader = Some(process);
        evidence.automatic = true;
        let automatic = observe(&launch, &adapters, true);
        let automatic_probes = probes.load(Ordering::Relaxed);
        launch.evidence.as_mut().unwrap().automatic = false;
        let mut foreign_launch = launch.clone();
        foreign_launch.host = "another-host".into();
        let foreign = observe(&foreign_launch, &adapters, true);
        let foreign_probes = probes.load(Ordering::Relaxed);
        let passive = observe(&launch, &adapters, false);
        assert_eq!(passive.cli, None);
        assert_eq!(probes.load(Ordering::Relaxed), 0);
        let manual = observe(&launch, &adapters, true);

        child.kill().unwrap();
        child.wait().unwrap();
        assert_eq!(automatic.cli, None);
        assert_eq!(automatic_probes, 0);
        assert_eq!(foreign.cli, None);
        assert_eq!(foreign_probes, 0);
        assert_eq!(manual.cli.as_deref(), Some("1.2.3"));
        assert_eq!(probes.load(Ordering::Relaxed), 1);
    }

    #[cfg(target_os = "linux")]
    fn observer_fixture(engine: BackendKind, migrate_during_probe: bool) {
        use std::sync::{Mutex, mpsc};
        use std::time::Duration;

        #[derive(Debug)]
        struct PausedAdapter {
            entered: mpsc::Sender<()>,
            resume: Mutex<mpsc::Receiver<()>>,
        }

        impl fl_core::VendorVersionAdapter for PausedAdapter {
            fn vendor_id(&self) -> &'static str {
                "codex"
            }

            fn version_from_executable(&self, executable: &Path) -> Option<String> {
                assert_eq!(executable, std::env::current_exe().unwrap());
                self.entered.send(()).unwrap();
                self.resume
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(3))
                    .unwrap();
                Some("2.0.0".into())
            }
        }

        #[derive(Debug)]
        struct ReadyAdapter;

        impl fl_core::VendorVersionAdapter for ReadyAdapter {
            fn vendor_id(&self) -> &'static str {
                "codex"
            }

            fn version_from_executable(&self, _executable: &Path) -> Option<String> {
                Some("2.0.0".into())
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(match engine {
            BackendKind::Ferro => "workspace.fls",
            BackendKind::Redb => "workspace.redb",
        });
        let make_state = |path: PathBuf, selection| {
            let mut state = crate::HubState::new(crate::HubServerConfig {
                store_path: path,
                home_override: Some(dir.path().into()),
                ..crate::HubServerConfig::default()
            });
            state.store_backend = Some(selection);
            state
        };
        let writer = make_state(path.clone(), engine);
        let observer = make_state(path.clone(), BackendKind::Ferro);
        let alias = dir.path().join(match engine {
            BackendKind::Ferro => "alias.fls",
            BackendKind::Redb => "alias.redb",
        });
        std::os::unix::fs::symlink(&path, &alias).unwrap();
        let competitor = make_state(alias, BackendKind::Ferro);

        let first = receipt(1);
        let mut second = receipt(2);
        second.captured_at_ms = now_ms().saturating_sub(SETTLE_MS);
        second.evidence.as_mut().unwrap().leader = ProcessIdentity::capture(std::process::id());
        let stream = second.stream.clone().unwrap();
        {
            let store = writer.open_store().unwrap();
            seed(&*store, &first);
            commit(&*store, first, sample(Some("1.0.0"), None), true).unwrap();
            seed(&*store, &second);
        }
        let (entered, on_probe) = mpsc::channel();
        let (resume_probe, resume) = mpsc::channel();
        let mut adapters = VendorVersionRegistry::default();
        adapters.register(PausedAdapter {
            entered,
            resume: Mutex::new(resume),
        });
        let mut events = observer.subscribe();
        let mut competitor_events = competitor.subscribe();
        let worker = std::thread::scope(|threads| {
            let first_worker = threads.spawn(|| {
                let mut worker = VersionObserver::default();
                worker.observe_with_adapters(&observer, &adapters).unwrap();
                worker
            });
            let timeout = Duration::from_secs(3);
            on_probe.recv_timeout(timeout).unwrap();
            // Neither the Hub lock nor a redb connection is held during the probe.
            assert!(observer.store_lock.try_lock().is_ok());
            {
                let store = writer.open_store().unwrap();
                assert!(
                    hosted_vendor_versions_for_stream(&*store, &stream)
                        .unwrap()
                        .is_none()
                );
            }
            if migrate_during_probe {
                let fls = path.with_extension("fls");
                assert!(fl_store::migrate_locked(&path, &fls).unwrap().is_clean());
            } else if engine == BackendKind::Redb {
                // A second redb observer may settle while the first has no open connection.
                // The older in-flight observation must then leave its warning alone.
                let mut ready = VendorVersionRegistry::default();
                ready.register(ReadyAdapter);
                VersionObserver::default()
                    .observe_with_adapters(&competitor, &ready)
                    .unwrap();
                assert!(competitor_events.try_recv().is_ok());
            } else {
                // A second Hub on a symlink alias must see the same ownership file.
                VersionObserver::default()
                    .observe_with_adapters(&competitor, &adapters)
                    .unwrap();
                assert!(on_probe.try_recv().is_err());

                // A direct CLI can settle passive evidence while the Hub's sample is paused.
                let mut ready = VendorVersionRegistry::default();
                ready.register(ReadyAdapter);
                let store = writer.open_store().unwrap();
                observe_direct_with_adapters(&*store, Some(&path), &[second.id], &ready).unwrap();
                let versions = hosted_vendor_versions_for_stream(&*store, &stream)
                    .unwrap()
                    .unwrap();
                assert_eq!(versions.cli.as_deref(), Some("2.0.0"));
            }
            resume_probe.send(()).unwrap();
            first_worker.join().unwrap()
        });
        assert_eq!(worker.ferro.is_some(), engine == BackendKind::Ferro);

        if migrate_during_probe {
            assert!(events.try_recv().is_err());
            let store = observer.open_store().unwrap();
            assert_eq!(store.kind(), BackendKind::Ferro);
            assert!(
                hosted_vendor_versions_for_stream(&*store, &stream)
                    .unwrap()
                    .is_none()
            );
            let (pending, _): (Vec<LaunchReceipt>, _) =
                load_revision(&*store, &queue_slug()).unwrap();
            assert_eq!(pending.len(), 1);
            assert_eq!(pending[0].id, second.id);
            return;
        }
        if engine == BackendKind::Ferro {
            assert_eq!(
                events.try_recv().unwrap().event,
                "session.vendor_version_changed"
            );
        } else {
            assert!(events.try_recv().is_err());
        }
        let store = writer.open_store().unwrap();
        let versions = hosted_vendor_versions_for_stream(&*store, &stream)
            .unwrap()
            .unwrap();
        assert_eq!(versions.cli.as_deref(), Some("2.0.0"));
        assert_eq!(versions.changes[0].previous, "1.0.0");
        assert_eq!(versions.changes[0].current, "2.0.0");
        if engine == BackendKind::Redb {
            assert!(!path.with_extension("fls").exists());
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn versions_observer_excludes_other_hubs_without_holding_the_hub_store_lock() {
        observer_fixture(BackendKind::Ferro, false);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn versions_observer_settles_lone_redb_without_migrating() {
        observer_fixture(BackendKind::Redb, false);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn versions_observer_does_not_commit_across_a_concurrent_migration() {
        observer_fixture(BackendKind::Redb, true);
    }
}
