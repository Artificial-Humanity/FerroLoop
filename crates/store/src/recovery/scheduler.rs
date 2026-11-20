//! Owner-handle workers and durable operation accounting. No policy can mint a
//! capture proof; activation remains gated on Task 7 evidence and review.
use super::{RecoveryError, control::*, policy::RecoveryPolicy, *};
use crate::FerroStore;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::File,
    io::{Read, Write},
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

struct OwnerFs {
    root: PathBuf,
    limits: OperationLimits,
    io: Arc<dyn fs::RecoveryIo>,
}
impl fs::RecoveryFs for OwnerFs {
    fn root(&self) -> &std::path::Path {
        &self.root
    }
    fn io(&self) -> Arc<dyn fs::RecoveryIo> {
        self.io.clone()
    }
    fn reconciliation_limits(&self) -> &OperationLimits {
        &self.limits
    }
}

/// Trusted in-process evidence. Intentionally neither Deserialize nor publicly
/// constructible: policy files and RPC flags cannot enable recurrence.
pub struct CaptureProof {
    build: String,
    engine: u32,
    schema: Option<u64>,
    profile: ValidationProfile,
    limits: policy::RecoveryLimits,
}
impl CaptureProof {
    fn covers(&self, policy: &RecoveryPolicy, owner: &FerroStore) -> bool {
        policy.limits.as_ref().is_some_and(|l| {
            current_build_identity().is_ok_and(|build| self.build == build)
                && self.engine == crate::ferrostore::RECOVERY_ENGINE
                && owner
                    .recovery_schema_version()
                    .is_ok_and(|schema| schema == self.schema)
                && self.profile == policy.profile
                && l.retained_pin_bytes <= self.limits.retained_pin_bytes
                && l.preparation_bytes <= self.limits.preparation_bytes
                && l.memory_bytes <= self.limits.memory_bytes
                && l.disk_bytes <= self.limits.disk_bytes
                && l.image_bytes <= self.limits.image_bytes
                && l.metadata_bytes <= self.limits.metadata_bytes
                && l.total_seconds <= self.limits.total_seconds
        })
    }
}
#[cfg(test)]
fn current_build_identity() -> Result<String, RecoveryError> {
    Ok("trusted-fixture-build".into())
}
#[cfg(not(test))]
fn current_build_identity() -> Result<String, RecoveryError> {
    use sha2::{Digest, Sha256};
    let path = std::env::current_exe().map_err(|_| refused("build identity unavailable"))?;
    let mut file = File::open(path).map_err(|_| refused("build identity unavailable"))?;
    let mut hash = Sha256::new();
    let mut buffer = [0; 65536];
    let mut total = 0u64;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let n = file
            .read(&mut buffer)
            .map_err(|_| refused("build identity unavailable"))?;
        if n == 0 {
            break;
        }
        total = total
            .checked_add(n as u64)
            .filter(|n| *n <= 512 << 20)
            .ok_or_else(|| refused("build identity byte limit"))?;
        if Instant::now() >= deadline {
            return Err(refused("build identity deadline"));
        }
        hash.update(&buffer[..n]);
    }
    Ok(format!("{:x}", hash.finalize()))
}
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    batches: BTreeMap<uuid::Uuid, BatchReport>,
    #[serde(default)]
    history_full: bool,
    #[serde(default)]
    consecutive_failures: u64,
    #[serde(default)]
    last_schedule_slot: Option<u64>,
    #[serde(default)]
    last_scheduled_success: Option<u64>,
    #[serde(default)]
    events: Vec<RecoveryEventEnvelope>,
    #[serde(default)]
    outbox_full: bool,
    #[serde(default)]
    deliveries: BTreeMap<String, EventDelivery>,
    #[serde(default)]
    clock: policy::SchedulerClock,
    #[serde(default)]
    scheduled: std::collections::BTreeSet<uuid::Uuid>,
    #[serde(default)]
    notified: std::collections::BTreeSet<uuid::Uuid>,
}
const JOURNAL_BYTES: usize = 1_048_576;
const HISTORY_LIMIT: usize = 1024;
struct BoundedEncoding(Vec<u8>);
impl Write for BoundedEncoding {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > JOURNAL_BYTES.saturating_sub(self.0.len()) {
            return Err(std::io::Error::other("journal byte limit"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl Journal {
    fn bounded_bytes(&mut self) -> Result<Vec<u8>, RecoveryError> {
        self.scheduled.retain(|id| self.batches.contains_key(id));
        self.notified.retain(|id| self.batches.contains_key(id));
        if self.batches.len() > HISTORY_LIMIT
            || self.events.len() > 32
            || self.deliveries.len() > 32
        {
            return Err(refused(
                "scheduler accounting limit; existing history retained",
            ));
        }
        let mut output = BoundedEncoding(Vec::new());
        serde_json::to_writer(&mut output, &self)
            .map_err(|_| refused("scheduler journal byte limit; existing history retained"))?;
        Ok(output.0)
    }
    // Terminal operation bindings are retained for idempotence. P0 has no
    // automatic journal purge; count/byte exhaustion refuses new admission
    // while UTC slots and aggregate failure/overdue evidence still persist.
    fn admits_history(&mut self) -> bool {
        self.batches.len() < HISTORY_LIMIT
            && self
                .bounded_bytes()
                .is_ok_and(|b| b.len() < JOURNAL_BYTES - (64 << 10))
    }
}

struct State {
    owner: Arc<FerroStore>,
    policy: RecoveryPolicy,
    proof: bool,
    journal: Mutex<Journal>,
    committed: Mutex<Journal>,
    persistence_failed: std::sync::atomic::AtomicBool,
    reconciling: std::sync::atomic::AtomicBool,
    directory: Option<fs::Dir>,
    _lock: Option<File>,
    io: Arc<dyn fs::RecoveryIo>,
    sink: Option<Arc<dyn RecoveryEventSink>>,
    sink_busy: Arc<std::sync::atomic::AtomicBool>,
    cancel: CancelToken,
    workers: Mutex<Vec<std::thread::JoinHandle<()>>>,
}
#[derive(Clone)]
pub struct RecoveryScheduler(Arc<State>);
fn refused(reason: &str) -> RecoveryError {
    RecoveryError::Refused(reason.into())
}
pub(super) fn private_file(path: &std::path::Path, create: bool) -> Result<File, RecoveryError> {
    let limits = OperationLimits::new(
        Instant::now() + Duration::from_secs(10),
        1 << 20,
        64 << 20,
        CancelToken::new(),
    );
    let (parent, name) = super::verify::private_parent(path, &limits)?;
    let file = private_file_at(&parent, &name, create)?;
    let (now, _) = super::verify::private_parent(path, &limits)?;
    let old = parent
        .metadata()
        .map_err(|_| refused("journal parent unavailable"))?;
    let current = now
        .metadata()
        .map_err(|_| refused("journal parent unavailable"))?;
    if (old.dev(), old.ino()) != (current.dev(), current.ino()) {
        return Err(refused("journal parent identity changed"));
    }
    Ok(file)
}
fn private_file_at(
    parent: &File,
    name: &std::ffi::CStr,
    create: bool,
) -> Result<File, RecoveryError> {
    use std::os::fd::{AsRawFd, FromRawFd};
    let flags = libc::O_RDWR
        | libc::O_NOFOLLOW
        | libc::O_CLOEXEC
        | libc::O_NONBLOCK
        | if create { libc::O_CREAT } else { 0 };
    let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags, 0o600) };
    if fd < 0 {
        return Err(refused("scheduler journal unavailable"));
    }
    let file = unsafe { File::from_raw_fd(fd) };
    let m = file
        .metadata()
        .map_err(|_| refused("scheduler journal metadata unavailable"))?;
    if !m.is_file()
        || m.permissions().mode() & 0o077 != 0
        || m.nlink() != 1
        || m.uid() != unsafe { libc::geteuid() }
    {
        return Err(refused(
            "scheduler journal must be owner-private regular file",
        ));
    }
    Ok(file)
}

fn emit_bounded(
    sink: Arc<dyn RecoveryEventSink>,
    envelope: RecoveryEventEnvelope,
    key: String,
    busy: Arc<std::sync::atomic::AtomicBool>,
    deadline: Instant,
) -> bool {
    use std::sync::atomic::Ordering;
    let (result, completion) = std::sync::mpsc::sync_channel(1);
    let flag = busy.clone();
    let spawned = std::thread::Builder::new()
        .name("fls-recovery-event".into())
        .spawn(move || {
            let delivered = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                sink.emit(&envelope, &key, deadline)
            }))
            .is_ok_and(|r| r.is_ok());
            flag.store(false, Ordering::Release);
            let _ = result.send(delivered);
        });
    if spawned.is_err() {
        busy.store(false, Ordering::Release);
        return false;
    }
    completion
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .unwrap_or(false)
        && Instant::now() < deadline
}

impl RecoveryScheduler {
    pub fn start(
        owner: FerroStore,
        policy: RecoveryPolicy,
        proof: Option<CaptureProof>,
    ) -> Result<Self, RecoveryError> {
        Self::start_shared(Arc::new(owner), policy, proof)
    }
    pub(crate) fn start_shared(
        owner: Arc<FerroStore>,
        policy: RecoveryPolicy,
        proof: Option<CaptureProof>,
    ) -> Result<Self, RecoveryError> {
        Self::start_shared_with_io(owner, policy, proof, Arc::new(fs::NativeRecoveryIo))
    }
    pub(crate) fn start_shared_with_io(
        owner: Arc<FerroStore>,
        policy: RecoveryPolicy,
        proof: Option<CaptureProof>,
        io: Arc<dyn fs::RecoveryIo>,
    ) -> Result<Self, RecoveryError> {
        Self::start_shared_with_runtime(owner, policy, proof, io, None)
    }
    pub fn start_with_sink(
        owner: FerroStore,
        policy: RecoveryPolicy,
        proof: Option<CaptureProof>,
        sink: Arc<dyn RecoveryEventSink>,
    ) -> Result<Self, RecoveryError> {
        Self::start_shared_with_runtime(
            Arc::new(owner),
            policy,
            proof,
            Arc::new(fs::NativeRecoveryIo),
            Some(sink),
        )
    }
    pub(crate) fn start_shared_with_runtime(
        owner: Arc<FerroStore>,
        policy: RecoveryPolicy,
        proof: Option<CaptureProof>,
        io: Arc<dyn fs::RecoveryIo>,
        sink: Option<Arc<dyn RecoveryEventSink>>,
    ) -> Result<Self, RecoveryError> {
        policy.validate()?;
        let proven = proof.as_ref().is_some_and(|p| p.covers(&policy, &owner));
        if policy.enabled && !proven {
            return Err(refused(
                "recurrence requires trusted capture proof; scheduling remains disabled",
            ));
        }
        if policy.enabled && sink.is_none() {
            return Err(refused(
                "recurrence requires a reviewed production event-sink adapter",
            ));
        }
        if policy.limits.is_none() {
            return Ok(Self(Arc::new(State {
                owner,
                policy,
                proof: false,
                journal: Mutex::new(Journal::default()),
                committed: Mutex::new(Journal::default()),
                persistence_failed: Default::default(),
                reconciling: Default::default(),
                directory: None,
                _lock: None,
                io,
                sink,
                sink_busy: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                cancel: CancelToken::new(),
                workers: Mutex::new(Vec::new()),
            })));
        }
        // Existing destination only. Neither policy loading nor control creates a backup root.
        let m = std::fs::symlink_metadata(&policy.destination)
            .map_err(|_| refused("configured recovery root unavailable"))?;
        if !m.is_dir()
            || m.permissions().mode() & 0o077 != 0
            || m.uid() != unsafe { libc::geteuid() }
        {
            return Err(refused(
                "configured recovery root must be owner-private directory",
            ));
        }
        let directory = policy
            .destination
            .canonicalize()
            .map_err(|_| refused("recovery root binding unavailable"))?;
        if directory != policy.destination {
            return Err(refused("canonical recovery root required"));
        }
        let name = super::inventory::metadata_digest(policy.store_id.as_bytes());
        let lock = private_file(&directory.join(format!(".scheduler-{name}.lock")), true)?;
        if unsafe {
            libc::flock(
                std::os::fd::AsRawFd::as_raw_fd(&lock),
                libc::LOCK_EX | libc::LOCK_NB,
            )
        } != 0
        {
            return Err(refused("another scheduler owns configured store"));
        }
        let journal_path = directory.join(format!(".scheduler-{name}.json"));
        let mut journal: Journal = match private_file(&journal_path, false) {
            Ok(file) => {
                let mut bytes = Vec::new();
                file.take(1_048_577)
                    .read_to_end(&mut bytes)
                    .map_err(|_| refused("journal read failed"))?;
                if bytes.len() > 1_048_576 {
                    return Err(refused("scheduler journal byte limit"));
                }
                serde_json::from_slice(&bytes).map_err(|_| refused("invalid scheduler journal"))?
            }
            Err(e) => match std::fs::symlink_metadata(&journal_path) {
                Err(missing) if missing.kind() == std::io::ErrorKind::NotFound => {
                    Journal::default()
                }
                _ => return Err(e),
            },
        };
        // Refuse oversized legacy/tampered accounting rather than discarding
        // unresolved IDs or pending events to make room.
        journal.bounded_bytes()?;
        let bounds = policy
            .limits
            .as_ref()
            .ok_or_else(|| refused("explicit limits required"))?;
        let directory = fs::Dir::open(
            &directory,
            &OperationLimits::new(
                Instant::now() + Duration::from_secs(bounds.total_seconds),
                bounds.disk_bytes,
                bounds.memory_bytes,
                CancelToken::new(),
            ),
        )?;
        let scheduler = Self(Arc::new(State {
            owner,
            policy,
            proof: proven,
            committed: Mutex::new(journal.clone()),
            journal: Mutex::new(journal),
            persistence_failed: Default::default(),
            reconciling: Default::default(),
            directory: Some(directory),
            _lock: Some(lock),
            io,
            sink,
            sink_busy: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            cancel: CancelToken::new(),
            workers: Mutex::new(Vec::new()),
        }));
        let initialized = (|| {
            scheduler.resume()?;
            scheduler.deliver_pending()?;
            #[cfg(not(test))]
            scheduler.start_timer()?;
            Ok(())
        })();
        scheduler.finish_start(initialized)
    }
    fn finish_start(self, initialized: Result<(), RecoveryError>) -> Result<Self, RecoveryError> {
        if let Err(error) = initialized {
            // Initialization may already have resumed interrupted work. Do not
            // let a server report recovery inactive while those workers survive.
            self.abort_start();
            return Err(error);
        }
        Ok(self)
    }
    fn abort_start(&self) {
        self.0.cancel.cancel();
        let workers = std::mem::take(
            &mut *self
                .0
                .workers
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        );
        // A panicked worker must not detach later workers during startup abort.
        for worker in workers {
            let _ = worker.join();
        }
    }
    #[cfg(not(test))]
    fn start_timer(&self) -> Result<(), RecoveryError> {
        if !self.0.policy.enabled || !self.0.policy.coordinator || !self.0.proof {
            return Ok(());
        }
        let weak = Arc::downgrade(&self.0);
        let timer = std::thread::Builder::new()
            .name("fls-recovery-timer".into())
            .spawn(move || {
                loop {
                    std::thread::sleep(Duration::from_millis(250));
                    let Some(state) = weak.upgrade() else {
                        break;
                    };
                    if state.cancel.is_cancelled() {
                        break;
                    }
                    let _ = Self(state).tick(std::time::SystemTime::now());
                }
            })
            .map_err(|_| refused("recovery timer unavailable"))?;
        self.0
            .workers
            .lock()
            .map_err(|_| refused("scheduler workers poisoned"))?
            .push(timer);
        Ok(())
    }
    fn filesystem(&self, limits: OperationLimits) -> OwnerFs {
        OwnerFs {
            root: self.0.policy.destination.clone(),
            limits,
            io: self.0.io.clone(),
        }
    }
    fn save(&self, journal: &mut Journal) -> Result<(), RecoveryError> {
        let bytes = match journal.bounded_bytes() {
            Ok(bytes) => bytes,
            Err(error) => {
                *journal = self
                    .0
                    .committed
                    .lock()
                    .map_err(|_| refused("scheduler state poisoned"))?
                    .clone();
                return Err(error);
            }
        };
        let result = self.save_inner(&bytes);
        let mut committed = self
            .0
            .committed
            .lock()
            .map_err(|_| refused("scheduler state poisoned"))?;
        match result {
            Ok(()) => {
                *committed = journal.clone();
                Ok(())
            }
            Err(error) => {
                // A rename may have reached disk even if directory sync failed.
                // Refuse further admission until restart reconciles that uncertainty.
                // Keep the conservative bounded candidate in memory; it may
                // already be the renamed disk image. Never release capacity.
                self.0
                    .persistence_failed
                    .store(true, std::sync::atomic::Ordering::Release);
                Err(error)
            }
        }
    }
    fn save_inner(&self, bytes: &[u8]) -> Result<(), RecoveryError> {
        use std::os::fd::AsRawFd;
        let directory = self
            .0
            .directory
            .as_ref()
            .ok_or_else(|| refused("recovery policy is absent"))?;
        let limits = self.accounting_limits(uuid::Uuid::nil())?;
        directory.same_name(&limits)?;
        let name = super::inventory::metadata_digest(self.0.policy.store_id.as_bytes());
        let temporary = std::ffi::CString::new(format!(".scheduler-{name}.tmp"))
            .map_err(|_| refused("journal name invalid"))?;
        let final_name = std::ffi::CString::new(format!(".scheduler-{name}.json"))
            .map_err(|_| refused("journal name invalid"))?;
        let mut file = private_file_at(&directory.file, &temporary, true)?;
        file.set_len(0)
            .and_then(|()| file.write_all(bytes))
            .and_then(|()| file.sync_all())
            .map_err(|_| refused("journal durability failed"))?;
        directory.same_name(&limits)?;
        let rc = unsafe {
            libc::renameat(
                directory.file.as_raw_fd(),
                temporary.as_ptr(),
                directory.file.as_raw_fd(),
                final_name.as_ptr(),
            )
        };
        if rc != 0 {
            return Err(refused("journal publication failed"));
        }
        directory
            .file
            .sync_all()
            .map_err(|_| refused("journal directory durability failed"))?;
        directory.same_name(&limits)
    }
    /// Cancels bounded recovery work and joins workers before the owner shuts down.
    pub(crate) fn shutdown(&self) -> Result<(), RecoveryError> {
        self.0.cancel.cancel();
        let workers = std::mem::take(
            &mut *self
                .0
                .workers
                .lock()
                .map_err(|_| refused("scheduler workers poisoned"))?,
        );
        for worker in workers {
            worker
                .join()
                .map_err(|_| refused("recovery worker panicked"))?;
        }
        Ok(())
    }
    fn resume(&self) -> Result<(), RecoveryError> {
        let mut journal = self
            .0
            .journal
            .lock()
            .map_err(|_| refused("scheduler state poisoned"))?;
        let pending: Vec<_> = journal
            .batches
            .values_mut()
            .filter(|b| {
                b.members.iter().any(|m| {
                    matches!(
                        m.status,
                        MemberStatus::Running | MemberStatus::OutcomeUnknown
                    )
                })
            })
            .map(|b| {
                for m in &mut b.members {
                    if m.status == MemberStatus::Running {
                        m.status = MemberStatus::OutcomeUnknown;
                    }
                }
                BatchRequest {
                    policy_id: self.0.policy.policy_id.clone(),
                    batch_id: b.batch_id,
                    members: b
                        .members
                        .iter()
                        .filter(|m| m.status == MemberStatus::OutcomeUnknown)
                        .map(|m| MemberRequest {
                            store_id: m.store_id.clone(),
                            operation_id: m.operation_id,
                            scheduled_at_unix_seconds: None,
                            byte_cap: Some(m.admitted_bytes),
                        })
                        .collect(),
                }
            })
            .collect();
        if !pending.is_empty() {
            self.save(&mut journal)?;
        }
        drop(journal);
        for mut request in pending {
            // Remote restart lookups share the bounded single-flight timer
            // pass. Only the interrupted physical owner's local operation
            // needs its own existing-handle reconciliation worker.
            request
                .members
                .retain(|m| m.store_id == self.0.policy.store_id);
            if request.members.is_empty() {
                continue;
            }
            let worker = self.clone();
            let handle = std::thread::Builder::new()
                .name("fls-recovery-reconcile".into())
                .spawn(move || worker.run(&request, true))
                .map_err(|_| refused("reconciliation worker unavailable"))?;
            self.0
                .workers
                .lock()
                .map_err(|_| refused("scheduler workers poisoned"))?
                .push(handle);
        }
        self.reconcile_background()?;
        Ok(())
    }
    fn reconcile_background(&self) -> Result<(), RecoveryError> {
        use std::sync::atomic::Ordering;
        if self.0.reconciling.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        let requests: Vec<_> = self
            .0
            .journal
            .lock()
            .map_err(|_| refused("scheduler state poisoned"))?
            .batches
            .values()
            .filter_map(|b| {
                let members: Vec<_> = b
                    .members
                    .iter()
                    .filter(|m| {
                        m.store_id != self.0.policy.store_id
                            && matches!(
                                m.status,
                                MemberStatus::Running | MemberStatus::OutcomeUnknown
                            )
                    })
                    .take(8)
                    .map(|m| MemberRequest {
                        store_id: m.store_id.clone(),
                        operation_id: m.operation_id,
                        scheduled_at_unix_seconds: None,
                        byte_cap: Some(m.admitted_bytes),
                    })
                    .collect();
                (!members.is_empty()).then_some(BatchRequest {
                    policy_id: self.0.policy.policy_id.clone(),
                    batch_id: b.batch_id,
                    members,
                })
            })
            .take(1)
            .collect();
        if requests.is_empty() {
            self.0.reconciling.store(false, Ordering::Release);
            return Ok(());
        }
        let worker = self.clone();
        let mut workers = self
            .0
            .workers
            .lock()
            .map_err(|_| refused("scheduler workers poisoned"))?;
        workers.retain(|w| !w.is_finished());
        if self.0.cancel.is_cancelled() {
            self.0.reconciling.store(false, Ordering::Release);
            return Ok(());
        }
        let spawned = std::thread::Builder::new()
            .name("fls-recovery-poll".into())
            .spawn(move || {
                let seconds = worker
                    .0
                    .policy
                    .limits
                    .as_ref()
                    .map_or(2, |l| l.total_seconds);
                let deadline = Instant::now() + Duration::from_secs(seconds);
                for request in requests {
                    worker.run_until(&request, true, deadline);
                }
                worker.0.reconciling.store(false, Ordering::Release);
            });
        match spawned {
            Ok(handle) => {
                workers.push(handle);
                Ok(())
            }
            Err(_) => {
                self.0.reconciling.store(false, Ordering::Release);
                Err(refused("reconciliation worker unavailable"))
            }
        }
    }
    /// Fake-clock seam used by owner timer and fixtures. Only a proven configured
    /// coordinator can originate a recurring batch. One attempt per UTC interval.
    pub fn tick(&self, now: std::time::SystemTime) -> Result<Option<BatchReport>, RecoveryError> {
        if self.0.cancel.is_cancelled()
            || self
                .0
                .persistence_failed
                .load(std::sync::atomic::Ordering::Acquire)
            || !self.0.policy.enabled
            || !self.0.policy.coordinator
            || !self.0.proof
        {
            return Ok(None);
        }
        self.reconcile_background()?;
        let now = now
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| refused("UTC scheduling time required"))?
            .as_secs();
        let mut journal = self
            .0
            .journal
            .lock()
            .map_err(|_| refused("scheduler state poisoned"))?;
        let due = journal.clock.due(&self.0.policy.members, now);
        if due.is_empty()
            || journal.batches.values().any(|b| {
                b.members.iter().any(|m| {
                    matches!(
                        m.status,
                        MemberStatus::Running | MemberStatus::OutcomeUnknown
                    )
                })
            })
        {
            return Ok(None);
        }
        journal.clock.admit(&due, now);
        let batch_id = uuid::Uuid::now_v7();
        journal.scheduled.insert(batch_id);
        self.save(&mut journal)?;
        drop(journal);
        let request = BatchRequest {
            policy_id: self.0.policy.policy_id.clone(),
            batch_id,
            members: due
                .into_iter()
                .map(|store_id| {
                    let interval = self
                        .0
                        .policy
                        .members
                        .iter()
                        .find(|m| m.store_id == store_id)
                        .expect("configured schedule")
                        .interval_seconds;
                    MemberRequest {
                        store_id,
                        operation_id: uuid::Uuid::now_v7(),
                        scheduled_at_unix_seconds: Some(now / interval * interval),
                        byte_cap: None,
                    }
                })
                .collect(),
        };
        match self.request_batch(&request) {
            Ok(report) => Ok(Some(report)),
            Err(_) => {
                let mut journal = self
                    .0
                    .journal
                    .lock()
                    .map_err(|_| refused("scheduler state poisoned"))?;
                if let Some(admitted) = journal.batches.get(&batch_id) {
                    return Ok(Some(admitted.clone()));
                }
                let mut report = BatchReport {
                    batch_id,
                    complete: false,
                    overdue: true,
                    outbox_full: journal.outbox_full,
                    missing_gates: self.missing_gates(),
                    inactive_reason: None,
                    members: request
                        .members
                        .iter()
                        .map(|m| MemberReport {
                            operation_id: m.operation_id,
                            store_id: m.store_id.clone(),
                            admitted_bytes: 0,
                            manifest_sha256: None,
                            status: MemberStatus::Failed,
                            sequence: None,
                            reason: Some("scheduled_admission_refused".into()),
                            catalog_identity: None,
                            dependencies: Vec::new(),
                            shared_config: None,
                        })
                        .collect(),
                };
                let keep_history = journal.admits_history();
                journal.history_full = !keep_history;
                journal.scheduled.insert(batch_id);
                journal.batches.insert(batch_id, report.clone());
                self.account_scheduled(&mut journal, batch_id);
                report.outbox_full = journal.outbox_full;
                journal.batches.insert(batch_id, report.clone());
                if !keep_history {
                    journal.batches.remove(&batch_id);
                    journal.scheduled.remove(&batch_id);
                    journal.notified.remove(&batch_id);
                }
                self.save(&mut journal)?;
                drop(journal);
                let _ = self.release_capacity(&request, &report);
                let _ = self.deliver_pending();
                Ok(Some(report))
            }
        }
    }
    pub(crate) fn activation_missing_gates(enabled: bool, proof: bool, sink: bool) -> Vec<String> {
        let mut gates = Vec::new();
        if !enabled {
            gates.push("recurrence_disabled".into());
        }
        if !proof {
            gates.push("capture_proof_missing".into());
        }
        if !sink {
            gates.push("reviewed_event_adapter_missing".into());
        }
        gates
    }
    fn missing_gates(&self) -> Vec<String> {
        Self::activation_missing_gates(self.0.policy.enabled, self.0.proof, self.0.sink.is_some())
    }
    fn accounting_limits(&self, operation: uuid::Uuid) -> Result<OperationLimits, RecoveryError> {
        let l = self
            .0
            .policy
            .limits
            .as_ref()
            .ok_or_else(|| refused("explicit limits required"))?;
        let mut limits = OperationLimits::new(
            Instant::now() + Duration::from_secs(l.total_seconds),
            l.disk_bytes,
            l.memory_bytes,
            self.0.cancel.clone(),
        );
        limits.operation_id = operation;
        Ok(limits)
    }
    fn admit_capacity(&self, request: &BatchRequest) -> Result<BatchRequest, RecoveryError> {
        let limits = self.accounting_limits(request.batch_id)?;
        let root = fs::Dir::open(&self.0.policy.destination, &limits)?;
        let unit = fs::allocation_geometry(&root, &limits)?;
        let l = self
            .0
            .policy
            .limits
            .as_ref()
            .ok_or_else(|| refused("explicit limits required"))?;
        let reserve = fs::overhead_bytes(unit, &limits)?
            .checked_add(l.metadata_bytes)
            .filter(|n| *n <= 512 << 20)
            .ok_or_else(|| refused("metadata reserve overflow"))?;
        let local_bound = reserve
            .checked_add(3 << 20)
            .and_then(|n| n.checked_add(l.image_bytes))
            .ok_or_else(|| refused("prospective bundle overflow"))?;
        let mut normalized = request.clone();
        for member in &mut normalized.members {
            let configured = if member.store_id == self.0.policy.store_id {
                local_bound
            } else {
                self.0
                    .policy
                    .members
                    .iter()
                    .find(|s| s.store_id == member.store_id)
                    .ok_or_else(|| refused("unconfigured capacity member"))?
                    .prospective_bundle_bytes
            };
            let cap = if member.store_id == self.0.policy.store_id {
                member.byte_cap.unwrap_or(configured)
            } else {
                member.byte_cap.unwrap_or(configured).min(configured)
            };
            if cap < configured {
                return Err(refused(
                    "owner configured bound exceeds requested admission cap",
                ));
            }
            member.byte_cap = Some(cap);
        }
        let config_bytes = if self.0.policy.configs.is_empty() {
            0
        } else {
            self.0
                .policy
                .configs
                .iter()
                .try_fold(4096u64, |n, c| n.checked_add(c.byte_limit))
                .and_then(|n| n.checked_add(fs::overhead_bytes(unit, &limits).ok()?))
                .ok_or_else(|| refused("batch config reserve overflow"))?
        };
        super::capacity::reserve(
            &root,
            &normalized,
            &self.0.policy.store_id,
            config_bytes,
            l.margin_bytes,
            self.0.io.clone(),
            &limits,
        )?;
        Ok(normalized)
    }
    fn release_capacity(
        &self,
        request: &BatchRequest,
        report: &BatchReport,
    ) -> Result<(), RecoveryError> {
        if report.members.iter().any(|m| {
            matches!(
                m.status,
                MemberStatus::Running | MemberStatus::OutcomeUnknown
            )
        }) {
            return Ok(());
        }
        let limits = self.accounting_limits(request.batch_id)?;
        let root = fs::Dir::open(&self.0.policy.destination, &limits)?;
        let full_request = BatchRequest {
            policy_id: request.policy_id.clone(),
            batch_id: report.batch_id,
            members: report
                .members
                .iter()
                .map(|m| MemberRequest {
                    store_id: m.store_id.clone(),
                    operation_id: m.operation_id,
                    scheduled_at_unix_seconds: None,
                    byte_cap: Some(m.admitted_bytes),
                })
                .collect(),
        };
        super::capacity::release(
            &root,
            &full_request,
            &self.0.policy.store_id,
            self.0.io.clone(),
            &limits,
        )
    }
    fn validate_request(&self, request: &BatchRequest) -> Result<(), RecoveryError> {
        if self.0.cancel.is_cancelled()
            || self
                .0
                .persistence_failed
                .load(std::sync::atomic::Ordering::Acquire)
            || self.0.policy.limits.is_none()
            || request.policy_id != self.0.policy.policy_id
            || request.batch_id.is_nil()
            || request.members.is_empty()
            || request.members.len() > 128
        {
            return Err(refused(
                "request does not select configured recovery policy",
            ));
        }
        let mut stores = std::collections::BTreeSet::new();
        let mut operations = std::collections::BTreeSet::new();
        for m in &request.members {
            if m.operation_id.is_nil()
                || !stores.insert(&m.store_id)
                || !operations.insert(m.operation_id)
                || (m.store_id != self.0.policy.store_id
                    && (!self.0.policy.coordinator
                        || !self
                            .0
                            .policy
                            .members
                            .iter()
                            .any(|s| s.store_id == m.store_id)))
            {
                return Err(refused(
                    "request contains unconfigured or repeated owner/operation",
                ));
            }
        }
        Ok(())
    }
    fn initial_report(&self, request: &BatchRequest, outbox_full: bool) -> BatchReport {
        BatchReport {
            batch_id: request.batch_id,
            complete: false,
            overdue: !self.0.proof,
            outbox_full,
            missing_gates: self.missing_gates(),
            inactive_reason: None,
            members: request
                .members
                .iter()
                .map(|m| MemberReport {
                    manifest_sha256: None,
                    admitted_bytes: m.byte_cap.unwrap_or(u64::MAX),
                    store_id: m.store_id.clone(),
                    operation_id: m.operation_id,
                    status: MemberStatus::Running,
                    sequence: None,
                    reason: None,
                    catalog_identity: None,
                    dependencies: Vec::new(),
                    shared_config: None,
                })
                .collect(),
        }
    }
    pub fn request_batch(&self, request: &BatchRequest) -> Result<BatchReport, RecoveryError> {
        self.validate_request(request)?;
        let mut journal = self
            .0
            .journal
            .lock()
            .map_err(|_| refused("scheduler state poisoned"))?;
        if let Some(old) = journal.batches.get(&request.batch_id) {
            if old.members.len() != request.members.len()
                || !request.members.iter().all(|m| {
                    old.members
                        .iter()
                        .any(|o| o.store_id == m.store_id && o.operation_id == m.operation_id)
                })
            {
                return Err(refused("batch ID is already bound to another member set"));
            }
            if request.members.iter().any(|m| {
                m.byte_cap.is_some_and(|cap| {
                    old.members
                        .iter()
                        .find(|o| o.operation_id == m.operation_id)
                        .is_none_or(|o| cap < o.admitted_bytes)
                })
            }) {
                return Err(refused("operation admission cap changed"));
            }
            return Ok(old.clone());
        }
        if request
            .members
            .iter()
            .any(|m| m.scheduled_at_unix_seconds.is_some())
        {
            if !self.0.policy.enabled || !self.0.proof || self.0.sink.is_none() {
                return Err(refused(
                    "scheduled request cannot bypass owner activation gates",
                ));
            }
            if let Some(slot) = request
                .members
                .iter()
                .find(|m| m.store_id == self.0.policy.store_id)
                .and_then(|m| m.scheduled_at_unix_seconds)
            {
                let interval = self.0.policy.coverage_interval_seconds;
                if interval == 0
                    || slot % interval != 0
                    || journal.last_schedule_slot.is_some_and(|old| old >= slot)
                {
                    return Err(refused("owner schedule slot already attempted or invalid"));
                }
            }
        }
        if journal.batches.values().any(|b| {
            b.members.iter().any(|m| {
                m.store_id == self.0.policy.store_id
                    && matches!(
                        m.status,
                        MemberStatus::Running | MemberStatus::OutcomeUnknown
                    )
            })
        }) {
            return Err(refused(
                "physical owner has an unresolved recovery operation",
            ));
        }
        if journal.batches.values().any(|b| {
            b.members.iter().any(|m| {
                request
                    .members
                    .iter()
                    .any(|r| r.operation_id == m.operation_id)
            })
        }) {
            return Err(refused("operation ID belongs to another batch"));
        }
        if !journal.admits_history() {
            return Err(refused(
                "scheduler history full; admission refused, existing operation bindings retained",
            ));
        }
        let mut prospective = journal.clone();
        prospective.batches.insert(
            request.batch_id,
            self.initial_report(request, journal.outbox_full),
        );
        if !prospective
            .bounded_bytes()
            .is_ok_and(|b| b.len() < JOURNAL_BYTES - (64 << 10))
        {
            return Err(refused(
                "scheduler history byte capacity exhausted; existing operation bindings retained",
            ));
        }
        let normalized = self.admit_capacity(request)?;
        let request = &normalized;
        if request
            .members
            .iter()
            .any(|m| m.scheduled_at_unix_seconds.is_some())
        {
            journal.scheduled.insert(request.batch_id);
        }
        if let Some(slot) = request
            .members
            .iter()
            .find(|m| m.store_id == self.0.policy.store_id)
            .and_then(|m| m.scheduled_at_unix_seconds)
        {
            journal.last_schedule_slot = Some(slot);
        }
        let report = self.initial_report(request, journal.outbox_full);
        journal.batches.insert(request.batch_id, report.clone());
        self.save(&mut journal)?;
        drop(journal);
        let worker = self.clone();
        let request = request.clone();
        let mut workers = self
            .0
            .workers
            .lock()
            .map_err(|_| refused("scheduler workers poisoned"))?;
        workers.retain(|w| !w.is_finished());
        if self.0.cancel.is_cancelled() {
            return Err(RecoveryError::Cancelled {
                operation_id: request.batch_id,
            });
        }
        let handle = std::thread::Builder::new()
            .name("fls-recovery".into())
            .spawn(move || worker.run(&request, false))
            .map_err(|_| refused("recovery worker unavailable"))?;
        workers.push(handle);
        Ok(report)
    }
    pub fn status(&self, request: &BatchRequest) -> Result<BatchReport, RecoveryError> {
        self.validate_request(request)?;
        let snapshot = self
            .0
            .journal
            .lock()
            .map_err(|_| refused("scheduler state poisoned"))?
            .batches
            .get(&request.batch_id)
            .cloned()
            .ok_or_else(|| refused("unknown recovery batch; coordinator coverage overdue"))?;
        if snapshot.members.len() != request.members.len()
            || !request.members.iter().all(|m| {
                snapshot
                    .members
                    .iter()
                    .any(|o| o.store_id == m.store_id && o.operation_id == m.operation_id)
            })
        {
            return Err(refused("batch member set mismatch"));
        }
        let mut updated = snapshot;
        for member in &mut updated.members {
            if member.store_id != self.0.policy.store_id
                && matches!(
                    member.status,
                    MemberStatus::Running | MemberStatus::OutcomeUnknown
                )
            {
                let endpoint = &self
                    .0
                    .policy
                    .members
                    .iter()
                    .find(|s| s.store_id == member.store_id)
                    .ok_or_else(|| refused("configured member missing"))?
                    .endpoint;
                let child = BatchRequest {
                    policy_id: request.policy_id.clone(),
                    batch_id: request.batch_id,
                    members: vec![MemberRequest {
                        store_id: member.store_id.clone(),
                        operation_id: member.operation_id,
                        scheduled_at_unix_seconds: None,
                        byte_cap: Some(member.admitted_bytes),
                    }],
                };
                if let Ok(report) = control::call(endpoint, "recovery_status", &child)
                    && let Some(result) = report.members.into_iter().find(|m| {
                        m.operation_id == member.operation_id && m.store_id == member.store_id
                    })
                {
                    *member = result;
                }
            }
        }
        let mut journal = self
            .0
            .journal
            .lock()
            .map_err(|_| refused("scheduler state poisoned"))?;
        let batch = journal
            .batches
            .get_mut(&request.batch_id)
            .ok_or_else(|| refused("batch disappeared"))?;
        for incoming in updated
            .members
            .into_iter()
            .filter(|m| m.store_id != self.0.policy.store_id)
        {
            if let Some(current) = batch.members.iter_mut().find(|m| {
                m.operation_id == incoming.operation_id && m.store_id == incoming.store_id
            }) && matches!(
                current.status,
                MemberStatus::Running | MemberStatus::OutcomeUnknown
            ) {
                *current = incoming;
            }
        }
        batch.refresh();
        if batch.complete && self.finalize_batch(batch).is_err() {
            batch.complete = false;
        }
        self.account_scheduled(&mut journal, request.batch_id);
        let outbox_full = journal.outbox_full;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| refused("UTC coverage time unavailable"))?
            .as_secs();
        let overdue = !self.0.policy.enabled
            || !self.0.proof
            || journal.last_scheduled_success.is_none_or(|last| {
                now.saturating_sub(last) > self.0.policy.coverage_interval_seconds
            });
        let batch = journal
            .batches
            .get_mut(&request.batch_id)
            .ok_or_else(|| refused("batch disappeared"))?;
        batch.outbox_full = outbox_full;
        batch.overdue = overdue;
        let updated = batch.clone();
        self.save(&mut journal)?;
        self.release_capacity(request, &updated)?;
        Ok(updated)
    }
    pub fn events(&self) -> Result<Vec<RecoveryEventEnvelope>, RecoveryError> {
        Ok(self
            .0
            .journal
            .lock()
            .map_err(|_| refused("scheduler state poisoned"))?
            .events
            .clone())
    }
    fn account_scheduled(&self, journal: &mut Journal, batch_id: uuid::Uuid) {
        if !journal.scheduled.contains(&batch_id) || journal.notified.contains(&batch_id) {
            return;
        }
        let Some(batch) = journal.batches.get(&batch_id) else {
            return;
        };
        if batch.members.iter().any(|m| {
            matches!(
                m.status,
                MemberStatus::Running | MemberStatus::OutcomeUnknown
            )
        }) {
            return;
        }
        journal.notified.insert(batch_id);
        if batch.complete {
            journal.consecutive_failures = 0;
            journal.last_scheduled_success = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .ok()
                .map(|d| d.as_secs());
            return;
        }
        journal.consecutive_failures = journal.consecutive_failures.saturating_add(1);
        let Some(failed) = batch
            .members
            .iter()
            .find(|m| m.status != MemberStatus::Durable)
        else {
            return;
        };
        let mut sinks = Vec::new();
        if let Some(sink) = &self.0.policy.failure_sink {
            sinks.push(sink.clone());
        }
        if journal.consecutive_failures == 2
            && let Some(sink) = &self.0.policy.owner_sink
        {
            sinks.push(sink.clone());
        }
        journal.outbox_full = false;
        for sink_id in sinks {
            // Bounded outbox accounting is included in the scheduler's explicit
            // metadata reservation. Delivered entries are the only eviction candidates.
            while journal.events.len() >= 32
                || serde_json::to_vec(&journal.events).map_or(true, |b| b.len() + 2048 > 65536)
            {
                let oldest = journal.events.iter().position(|e| {
                    journal
                        .deliveries
                        .get(&format!("{}:{}", e.event.operation_id, e.sink_id))
                        .is_some_and(|d| d.status == DeliveryStatus::Delivered)
                });
                if let Some(index) = oldest {
                    let old = journal.events.remove(index);
                    journal
                        .deliveries
                        .remove(&format!("{}:{}", old.event.operation_id, old.sink_id));
                } else {
                    journal.outbox_full = true;
                    break;
                }
            }
            if journal.outbox_full {
                continue;
            }
            let catalog_identity = failed.catalog_identity.clone().unwrap_or_else(|| {
                if failed.store_id == self.0.policy.store_id {
                    self.0.policy.profile.catalog_identity.clone()
                } else {
                    self.0
                        .policy
                        .members
                        .iter()
                        .find(|m| m.store_id == failed.store_id)
                        .map(|m| m.catalog_identity.clone())
                        .unwrap_or_else(|| "unknown".into())
                }
            });
            let delivery = EventDelivery {
                attempts: 0,
                status: DeliveryStatus::Pending,
            };
            let envelope = RecoveryEventEnvelope {
                batch_id,
                sink_id,
                event: RecoveryEvent {
                    operation_id: failed.operation_id,
                    catalog_identity,
                    outcome: RecoveryEventOutcome {
                        code: RecoveryOutcomeCode::ScheduledOwnerFailure,
                        consecutive_failures: journal.consecutive_failures,
                    },
                    sizes: RecoverySizes::default(),
                },
            };
            // Charge each envelope plus delivery record a fixed 2 KiB in the
            // 64 KiB outbox portion of the 3 MiB scheduler metadata reserve R.
            if serde_json::to_vec(&(&envelope, &delivery)).map_or(true, |bytes| bytes.len() > 2048)
            {
                journal.outbox_full = true;
                continue;
            }
            journal
                .deliveries
                .entry(format!("{}:{}", failed.operation_id, envelope.sink_id))
                .or_insert(delivery);
            journal.events.push(envelope);
        }
    }
    /// A complete set receives an immutable checksum-bound batch record only
    /// after independently rechecking every member and receipt under this root.
    fn finalize_batch(&self, batch: &mut BatchReport) -> Result<(), RecoveryError> {
        let deadline = self.accounting_limits(batch.batch_id)?.deadline;
        self.finalize_batch_until(batch, deadline)
    }
    fn finalize_batch_until(
        &self,
        batch: &mut BatchReport,
        deadline: Instant,
    ) -> Result<(), RecoveryError> {
        let mut limits = self.accounting_limits(batch.batch_id)?;
        limits.deadline = limits.deadline.min(deadline);
        limits.check()?;
        // Completed batch records are material catalog references too. Keep the
        // shared lock from member revalidation through durable publication.
        let catalog_root = fs::Dir::open(&self.0.policy.destination, &limits)?;
        let _catalog_access = fs::CatalogLock::acquire(
            &catalog_root,
            fs::CatalogLockMode::Shared,
            true,
            self.0.io.clone(),
            &limits,
        )?;
        for member in &mut batch.members {
            let bundle = backup::VerifiedBundle {
                path: self
                    .0
                    .policy
                    .destination
                    .join(member.operation_id.to_string()),
            };
            let manifest = super::catalog::retention_manifest(&bundle, &limits)?;
            if manifest.operation_id != member.operation_id.to_string()
                || manifest.batch_id.as_deref() != Some(batch.batch_id.to_string().as_str())
                || member.sequence != Some(manifest.published_sequence)
                || member.catalog_identity.as_ref() != Some(&manifest.catalog_store_identity)
                || member.dependencies != manifest.reference_dependencies
                || member.shared_config != manifest.shared_config
            {
                return Err(limits.verification());
            }
            let file = super::verify::PrivateFile::open(
                &bundle.path().join("manifest.json"),
                false,
                &limits,
            )?;
            let (_, digest) = super::verify::hash_file(&file.file, &limits)?;
            file.unchanged(&limits)?;
            member.manifest_sha256 = Some(digest);
        }
        batch.refresh();
        if !batch.complete {
            return Err(refused("batch dependency set incomplete"));
        }
        if !self.0.policy.coordinator {
            return Ok(());
        }
        let root = fs::Dir::open(&self.0.policy.destination, &limits)?;
        let name = format!(".batch-{}.json", batch.batch_id);
        let completion = DurableBatchV1 {
            version: 1,
            batch_id: batch.batch_id,
            members: batch
                .members
                .iter()
                .map(|m| {
                    Ok(DurableBatchMember {
                        operation_id: m.operation_id,
                        catalog_identity: m
                            .catalog_identity
                            .clone()
                            .ok_or_else(|| limits.verification())?,
                        sequence: m.sequence.ok_or_else(|| limits.verification())?,
                        manifest_sha256: m
                            .manifest_sha256
                            .clone()
                            .ok_or_else(|| limits.verification())?,
                        shared_config: m.shared_config.clone(),
                    })
                })
                .collect::<Result<Vec<_>, RecoveryError>>()?,
        };
        let bytes =
            serde_json::to_vec(&completion).map_err(|_| refused("batch record encoding failed"))?;
        if bytes.len() > 65536 {
            return Err(refused("batch record byte limit"));
        }
        if root.exists(&name, &limits)? {
            let held = super::verify::PrivateFile::open(&root.path.join(&name), false, &limits)?;
            let mut old = Vec::new();
            (&held.file)
                .take(65537)
                .read_to_end(&mut old)
                .map_err(|_| refused("batch record unavailable"))?;
            if old != bytes {
                return Err(limits.verification());
            }
            held.file
                .sync_all()
                .map_err(|_| refused("batch record durability failed"))?;
            held.unchanged(&limits)?;
        } else {
            let temporary = format!(".batch-{}.tmp", batch.batch_id);
            let mut file = private_file(&root.path.join(&temporary), true)?;
            file.set_len(0)
                .and_then(|()| file.write_all(&bytes))
                .and_then(|()| file.sync_all())
                .map_err(|_| refused("batch record durability failed"))?;
            root.rename(
                &temporary,
                &root,
                &name,
                &*self.0.io,
                fs::RecoveryEvent::CatalogPublish,
                &limits,
            )?;
        }
        root.sync(&*self.0.io, fs::Directory::CatalogParent, &limits)
    }
    /// Two attempts total per stable operation/sink key, including restarts.
    /// Missing adapters leave durable pending records and never claim delivery.
    fn deliver_pending(&self) -> Result<(), RecoveryError> {
        self.deliver_pending_until(Instant::now() + Duration::from_secs(2))
    }
    fn deliver_pending_until(&self, deadline: Instant) -> Result<(), RecoveryError> {
        let Some(sink) = &self.0.sink else {
            return Ok(());
        };
        let events = self.events()?;
        for event in events {
            let key = format!("{}:{}", event.event.operation_id, event.sink_id);
            loop {
                if Instant::now() >= deadline {
                    return Ok(());
                }
                let mut journal = self
                    .0
                    .journal
                    .lock()
                    .map_err(|_| refused("scheduler state poisoned"))?;
                let delivery = journal
                    .deliveries
                    .entry(key.clone())
                    .or_insert(EventDelivery {
                        attempts: 0,
                        status: DeliveryStatus::Pending,
                    });
                if delivery.status == DeliveryStatus::Delivered || delivery.attempts >= 2 {
                    break;
                }
                use std::sync::atomic::Ordering;
                if self.0.sink_busy.swap(true, Ordering::AcqRel) {
                    break;
                }
                delivery.attempts += 1;
                if let Err(error) = self.save(&mut journal) {
                    self.0.sink_busy.store(false, Ordering::Release);
                    return Err(error);
                }
                drop(journal);
                let delivered = emit_bounded(
                    sink.clone(),
                    event.clone(),
                    key.clone(),
                    self.0.sink_busy.clone(),
                    deadline.min(Instant::now() + Duration::from_secs(2)),
                );
                let mut journal = self
                    .0
                    .journal
                    .lock()
                    .map_err(|_| refused("scheduler state poisoned"))?;
                if delivered && let Some(delivery) = journal.deliveries.get_mut(&key) {
                    delivery.status = DeliveryStatus::Delivered;
                }
                self.save(&mut journal)?;
            }
        }
        Ok(())
    }
    fn shared_reference(
        &self,
        batch: uuid::Uuid,
        l: &OperationLimits,
    ) -> Result<Option<manifest::SharedConfigReference>, RecoveryError> {
        let path = self
            .0
            .policy
            .destination
            .join(".batch-config")
            .join(batch.to_string());
        match std::fs::symlink_metadata(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(_) => Err(refused("batch config unavailable")),
            Ok(_) => {
                let root = fs::Dir::open(&self.0.policy.destination, l)?;
                let descriptor =
                    super::verify::PrivateFile::open(&path.join("config.json"), false, l)?;
                let (_, digest) = super::verify::hash_file(&descriptor.file, l)?;
                let reference = manifest::SharedConfigReference {
                    batch_id: batch,
                    descriptor_sha256: digest,
                };
                super::catalog::check_shared_config(
                    &root,
                    &reference,
                    Some(&batch.to_string()),
                    l,
                )?;
                descriptor.unchanged(l)?;
                Ok(Some(reference))
            }
        }
    }
    fn prepare_config(&self, batch: uuid::Uuid) -> Result<(), RecoveryError> {
        if self.0.policy.configs.is_empty() {
            return Ok(());
        }
        let mut request = self.make_request(batch, batch)?;
        request.configs = self.0.policy.configs.clone();
        request.excluded_vault_key = self.0.policy.excluded_vault_key.clone();
        let filesystem = self.filesystem(request.limits.clone());
        backup::publish_batch_configs(&request, &filesystem)?;
        Ok(())
    }
    fn run(&self, request: &BatchRequest, reconcile: bool) {
        let seconds = self.0.policy.limits.as_ref().map_or(2, |l| l.total_seconds);
        self.run_until(
            request,
            reconcile,
            Instant::now() + Duration::from_secs(seconds),
        );
    }
    fn run_until(&self, request: &BatchRequest, reconcile: bool, deadline: Instant) {
        let config_ready = reconcile || self.prepare_config(request.batch_id).is_ok();
        for member in &request.members {
            if reconcile && Instant::now() >= deadline {
                break;
            }
            let result = if !config_ready {
                Err(refused("batch configuration publication failed"))
            } else if member.store_id == self.0.policy.store_id {
                self.local_until(request.batch_id, member.operation_id, reconcile, deadline)
            } else {
                let endpoint = &self
                    .0
                    .policy
                    .members
                    .iter()
                    .find(|s| s.store_id == member.store_id)
                    .expect("validated member")
                    .endpoint;
                let child = BatchRequest {
                    policy_id: request.policy_id.clone(),
                    batch_id: request.batch_id,
                    members: vec![member.clone()],
                };
                control::call_until(
                    endpoint,
                    if reconcile {
                        "recovery_status"
                    } else {
                        "recovery_start"
                    },
                    &child,
                    deadline.min(Instant::now() + Duration::from_secs(2)),
                )
                .map_err(|error| match error {
                    control::OwnerCallError::NotDispatched if !reconcile => {
                        refused("owner not contacted")
                    }
                    _ => RecoveryError::OutcomeUnknown {
                        operation_id: member.operation_id,
                    },
                })
                .and_then(|r| {
                    r.members
                        .into_iter()
                        .find(|m| {
                            m.store_id == member.store_id && m.operation_id == member.operation_id
                        })
                        .ok_or(RecoveryError::OutcomeUnknown {
                            operation_id: member.operation_id,
                        })
                })
            };
            let mut report = result.unwrap_or_else(|error| MemberReport {
                manifest_sha256: None,
                admitted_bytes: 0,
                store_id: member.store_id.clone(),
                operation_id: member.operation_id,
                status: if matches!(error, RecoveryError::OutcomeUnknown { .. }) {
                    MemberStatus::OutcomeUnknown
                } else {
                    MemberStatus::Failed
                },
                sequence: None,
                reason: Some(
                    if matches!(error, RecoveryError::OutcomeUnknown { .. }) {
                        "owner_outcome_unknown"
                    } else {
                        "owner_capture_or_publication_failed"
                    }
                    .into(),
                ),
                catalog_identity: None,
                dependencies: Vec::new(),
                shared_config: None,
            });
            report.admitted_bytes = member.byte_cap.unwrap_or(0);
            if let Ok(mut journal) = self.0.journal.lock() {
                if let Some(batch) = journal.batches.get_mut(&request.batch_id) {
                    if let Some(slot) = batch
                        .members
                        .iter_mut()
                        .find(|m| m.operation_id == member.operation_id)
                        && matches!(
                            slot.status,
                            MemberStatus::Running | MemberStatus::OutcomeUnknown
                        )
                    {
                        *slot = report;
                    }
                    batch.refresh();
                    if batch.complete && self.finalize_batch_until(batch, deadline).is_err() {
                        batch.complete = false;
                    }
                }
                self.account_scheduled(&mut journal, request.batch_id);
                if self.save(&mut journal).is_ok()
                    && let Some(batch) = journal.batches.get(&request.batch_id)
                {
                    let _ = self.release_capacity(request, batch);
                }
            }
            let _ =
                self.deliver_pending_until(deadline.min(Instant::now() + Duration::from_secs(2)));
        }
    }
    fn make_request(
        &self,
        batch: uuid::Uuid,
        operation: uuid::Uuid,
    ) -> Result<backup::BackupRequest, RecoveryError> {
        let policy = &self.0.policy;
        let l = policy
            .limits
            .as_ref()
            .ok_or_else(|| refused("explicit limits required"))?;
        let deadline = Instant::now() + Duration::from_secs(l.total_seconds);
        let source = self.0.owner.recovery_source_path(operation)?;
        let inventory = inventory::collect_inventory(&inventory::InventoryInputs {
            roots: vec![
                source
                    .parent()
                    .ok_or_else(|| refused("source parent unavailable"))?
                    .to_path_buf(),
            ],
            bindings: vec![inventory::BindingInput::config(
                &policy.store_id,
                source.to_path_buf(),
                Some("ferro".into()),
                "configured owner",
            )],
            ..Default::default()
        })?;
        let identity = inventory
            .stores
            .into_iter()
            .find(|s| s.identity.canonical_path.as_deref() == Some(source))
            .ok_or_else(|| refused("owner inventory unavailable"))?
            .identity;
        if Instant::now() >= deadline {
            return Err(RecoveryError::Deadline {
                operation_id: operation,
            });
        }
        let mut limits = OperationLimits::new(
            deadline,
            l.disk_bytes,
            l.memory_bytes,
            self.0.cancel.clone(),
        );
        limits.operation_id = operation;
        let profile = policy.profile.clone();
        if profile.catalog_identity != identity.catalog_key {
            return Err(refused(
                "configured validation profile does not bind owner identity",
            ));
        }
        Ok(backup::BackupRequest {
            store: identity,
            source_map_sha256: policy.source_map_sha256.clone(),
            operation_id: operation,
            batch_id: Some(batch),
            destination: policy.destination.clone(),
            capture: CaptureBudget::new(
                deadline,
                l.retained_pin_bytes,
                l.preparation_bytes,
                Duration::from_secs(1),
            ),
            limits: limits.clone(),
            profile,
            image_byte_limit: Some(l.image_bytes),
            metadata_byte_limit: l.metadata_bytes,
            space_margin_bytes: l.margin_bytes,
            configs: vec![],
            excluded_vault_key: None,
            code_index_configuration: manifest::Availability::Unavailable,
        })
    }
    fn report_bundle(
        &self,
        batch: uuid::Uuid,
        operation: uuid::Uuid,
        bundle: &backup::VerifiedBundle,
        limits: &OperationLimits,
    ) -> Result<MemberReport, RecoveryError> {
        let manifest = super::catalog::retention_manifest(bundle, limits)?;
        if manifest.operation_id != operation.to_string()
            || manifest.batch_id.as_deref() != Some(batch.to_string().as_str())
            || manifest.catalog_store_identity != self.0.policy.profile.catalog_identity
        {
            return Err(limits.verification());
        }
        Ok(MemberReport {
            manifest_sha256: None,
            admitted_bytes: 0,
            store_id: self.0.policy.store_id.clone(),
            operation_id: operation,
            status: MemberStatus::Durable,
            sequence: Some(manifest.published_sequence),
            reason: None,
            catalog_identity: Some(manifest.catalog_store_identity),
            dependencies: manifest.reference_dependencies,
            shared_config: manifest.shared_config,
        })
    }
    #[cfg(test)]
    fn local(
        &self,
        batch: uuid::Uuid,
        operation: uuid::Uuid,
        reconcile: bool,
    ) -> Result<MemberReport, RecoveryError> {
        self.local_until(
            batch,
            operation,
            reconcile,
            self.accounting_limits(operation)?.deadline,
        )
    }
    fn local_until(
        &self,
        batch: uuid::Uuid,
        operation: uuid::Uuid,
        reconcile: bool,
        deadline: Instant,
    ) -> Result<MemberReport, RecoveryError> {
        let policy = &self.0.policy;
        let mut request = self.make_request(batch, operation)?;
        request.limits.deadline = request.limits.deadline.min(deadline);
        request.capture.deadline = request.capture.deadline.min(deadline);
        request.limits.check()?;
        let filesystem = self.filesystem(request.limits.clone());
        let shared = self.shared_reference(batch, &request.limits)?;
        let outcome = if reconcile {
            backup::reconcile_publication(&operation.to_string(), &filesystem)?
        } else {
            backup::backup_with_shared_config(
                &self.0.owner,
                &request,
                shared.as_ref(),
                &filesystem,
                &self.0.cancel,
            )?
        };
        match outcome {
            backup::PublicationOutcome::Durable(bundle) => {
                self.report_bundle(batch, operation, &bundle, &request.limits)
            }
            backup::PublicationOutcome::OutcomeUnknown { .. } => Ok(MemberReport {
                manifest_sha256: None,
                admitted_bytes: 0,
                store_id: policy.store_id.clone(),
                operation_id: operation,
                status: MemberStatus::OutcomeUnknown,
                sequence: None,
                reason: Some("publication_outcome_unknown".into()),
                catalog_identity: None,
                dependencies: Vec::new(),
                shared_config: None,
            }),
        }
    }
}

#[cfg(test)]
fn fixture_directory() -> tempfile::TempDir {
    let path = std::env::var_os("FL_RECOVERY_TEST_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let directory = tempfile::tempdir_in(path)
        .expect("FL_RECOVERY_TEST_DIR must select writable native ext4/XFS fixture storage");
    #[cfg(target_os = "linux")]
    {
        let mut stat = std::mem::MaybeUninit::<libc::statfs>::uninit();
        let path = std::ffi::CString::new(directory.path().as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::statfs(path.as_ptr(), stat.as_mut_ptr()) }, 0);
        assert!(
            matches!(
                unsafe { stat.assume_init() }.f_type as u64,
                0xef53 | 0x58465342
            ),
            "scheduler fixtures require native ext4/XFS: set FL_RECOVERY_TEST_DIR; tmpfs is not a passing refusal test"
        );
    }
    directory
}

#[cfg(test)]
mod tests {
    use super::*;
    pub(super) fn policy(root: &std::path::Path) -> RecoveryPolicy {
        RecoveryPolicy {
            enabled: true,
            coordinator: true,
            coverage_interval_seconds: 3600,
            policy_id: "fixture".into(),
            store_id: "workspace".into(),
            destination: root.to_path_buf(),
            source_map_sha256: "a".repeat(64),
            members: vec![policy::Schedule {
                store_id: "unserved_council".into(),
                endpoint: format!("unix://{}/absent.sock", root.display()),
                interval_seconds: 3600,
                prospective_bundle_bytes: 32 << 20,
                catalog_identity: "catalog:fixture".into(),
            }],
            limits: Some(policy::RecoveryLimits {
                retained_pin_bytes: 1 << 20,
                preparation_bytes: 2 << 20,
                memory_bytes: 32 << 20,
                disk_bytes: 64 << 20,
                image_bytes: 4096,
                metadata_bytes: 128 << 10,
                margin_bytes: 4096,
                total_seconds: 30,
            }),
            failure_sink: Some("inert:ferris".into()),
            owner_sink: Some("inert:owner".into()),
            ..RecoveryPolicy::default()
        }
    }
    pub(super) fn proof(p: &RecoveryPolicy) -> CaptureProof {
        CaptureProof {
            build: current_build_identity().unwrap(),
            engine: crate::ferrostore::RECOVERY_ENGINE,
            schema: Some(crate::FORMAT_WITH_WORKFLOW),
            profile: p.profile.clone(),
            limits: p.limits.clone().unwrap(),
        }
    }
    #[derive(Default)]
    pub(super) struct InertSink {
        keys: Mutex<std::collections::BTreeSet<String>>,
    }
    impl RecoveryEventSink for InertSink {
        fn emit(
            &self,
            _: &RecoveryEventEnvelope,
            key: &str,
            _: Instant,
        ) -> Result<(), RecoveryError> {
            self.keys.lock().unwrap().insert(key.into());
            Ok(())
        }
    }
    pub(super) fn settle(s: &RecoveryScheduler) {
        let workers = std::mem::take(&mut *s.0.workers.lock().unwrap());
        for worker in workers {
            worker.join().unwrap();
        }
    }
    #[test]
    fn failed_initialization_cancels_and_joins_already_started_recovery_worker() {
        let scheduler =
            RecoveryScheduler::start(FerroStore::in_memory(), RecoveryPolicy::default(), None)
                .unwrap();
        let state = scheduler.0.clone();
        let cancel = state.cancel.clone();
        let joined = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let done = joined.clone();
        let worker = std::thread::spawn(move || {
            while !cancel.is_cancelled() {
                std::thread::park_timeout(Duration::from_millis(1));
            }
            done.store(true, std::sync::atomic::Ordering::Release);
        });
        state
            .workers
            .lock()
            .unwrap()
            .push(std::thread::spawn(|| panic!("injected first-worker panic")));
        state.workers.lock().unwrap().push(worker);
        let result = scheduler.finish_start(Err(refused("injected post-resume startup failure")));
        let was_cancelled = state.cancel.is_cancelled();
        let was_joined = joined.load(std::sync::atomic::Ordering::Acquire);
        let no_workers = state.workers.lock().unwrap().is_empty();
        // Always release the fixture worker even if a cleanup mutation survives.
        state.cancel.cancel();
        for worker in std::mem::take(&mut *state.workers.lock().unwrap()) {
            let _ = worker.join();
        }
        assert!(
            matches!(result, Err(RecoveryError::Refused(reason)) if reason == "injected post-resume startup failure")
        );
        assert!(was_cancelled && was_joined && no_workers);
    }
    #[test]
    fn capture_proof_and_event_adapter_are_independent_activation_gates() {
        for with_proof in [false, true] {
            for with_sink in [false, true] {
                let root = fixture_directory();
                std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700))
                    .unwrap();
                let mut p = policy(root.path());
                let capture_proof = with_proof.then(|| proof(&p));
                let sink =
                    with_sink.then(|| Arc::new(InertSink::default()) as Arc<dyn RecoveryEventSink>);
                let result = RecoveryScheduler::start_shared_with_runtime(
                    Arc::new(FerroStore::in_memory()),
                    p.clone(),
                    capture_proof,
                    Arc::new(fs::NativeRecoveryIo),
                    sink,
                );
                if !with_proof {
                    assert!(
                        matches!(result, Err(RecoveryError::Refused(ref reason)) if reason.contains("trusted capture proof"))
                    );
                } else if !with_sink {
                    assert!(
                        matches!(result, Err(RecoveryError::Refused(ref reason)) if reason.contains("event-sink adapter"))
                    );
                } else {
                    let scheduler = result.unwrap();
                    assert!(
                        scheduler
                            .initial_report(
                                &BatchRequest {
                                    policy_id: "fixture".into(),
                                    batch_id: uuid::Uuid::now_v7(),
                                    members: vec![],
                                },
                                false
                            )
                            .missing_gates
                            .is_empty()
                    );
                    scheduler.shutdown().unwrap();
                }
                let expected_enabled: Vec<String> = [
                    (!with_proof).then_some("capture_proof_missing"),
                    (!with_sink).then_some("reviewed_event_adapter_missing"),
                ]
                .into_iter()
                .flatten()
                .map(str::to_owned)
                .collect();
                assert_eq!(
                    RecoveryScheduler::activation_missing_gates(true, with_proof, with_sink),
                    expected_enabled
                );
                // The same gate combinations are visible in a dormant owner's status.
                p.enabled = false;
                let scheduler = RecoveryScheduler::start_shared_with_runtime(
                    Arc::new(FerroStore::in_memory()),
                    p.clone(),
                    with_proof.then(|| proof(&p)),
                    Arc::new(fs::NativeRecoveryIo),
                    with_sink.then(|| Arc::new(InertSink::default()) as Arc<dyn RecoveryEventSink>),
                )
                .unwrap();
                let mut expected = vec!["recurrence_disabled"];
                if !with_proof {
                    expected.push("capture_proof_missing");
                }
                if !with_sink {
                    expected.push("reviewed_event_adapter_missing");
                }
                assert_eq!(
                    scheduler
                        .initial_report(
                            &BatchRequest {
                                policy_id: "fixture".into(),
                                batch_id: uuid::Uuid::now_v7(),
                                members: vec![],
                            },
                            false
                        )
                        .missing_gates,
                    expected
                );
            }
        }
    }
    #[test]
    fn scheduled_request_cannot_bypass_disabled_policy_even_with_proof_and_sink() {
        let root = fixture_directory();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut p = policy(root.path());
        p.enabled = false;
        let capture_proof = proof(&p);
        let scheduler = RecoveryScheduler::start_with_sink(
            FerroStore::in_memory(),
            p,
            Some(capture_proof),
            Arc::new(InertSink::default()),
        )
        .unwrap();
        let before = std::fs::read_dir(root.path()).unwrap().count();
        let request = BatchRequest {
            policy_id: "fixture".into(),
            batch_id: uuid::Uuid::now_v7(),
            members: vec![MemberRequest {
                store_id: "unserved_council".into(),
                operation_id: uuid::Uuid::now_v7(),
                scheduled_at_unix_seconds: Some(3600),
                byte_cap: None,
            }],
        };
        assert!(
            matches!(scheduler.request_batch(&request), Err(RecoveryError::Refused(reason)) if reason == "scheduled request cannot bypass owner activation gates")
        );
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), before);
        assert!(scheduler.0.journal.lock().unwrap().batches.is_empty());
    }
    #[test]
    fn no_client_policy_can_mint_proof_and_disabled_tick_has_no_side_effects() {
        let root = fixture_directory();
        let p = policy(root.path());
        assert!(RecoveryScheduler::start(FerroStore::in_memory(), p, None).is_err());
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
        let s = RecoveryScheduler::start(FerroStore::in_memory(), RecoveryPolicy::default(), None)
            .unwrap();
        assert!(
            s.tick(std::time::UNIX_EPOCH + Duration::from_secs(21600))
                .unwrap()
                .is_none()
        );
    }
    #[test]
    fn failed_scheduled_runs_emit_inert_events_once_and_poll_does_not_reset() {
        let root = fixture_directory();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let p = policy(root.path());
        let proof = proof(&p);
        let sink = Arc::new(InertSink::default());
        let s = RecoveryScheduler::start_with_sink(
            FerroStore::in_memory(),
            p,
            Some(proof),
            sink.clone(),
        )
        .unwrap();
        let first = s
            .tick(std::time::UNIX_EPOCH + Duration::from_secs(3600))
            .unwrap()
            .unwrap();
        settle(&s);
        assert!(
            s.tick(std::time::UNIX_EPOCH + Duration::from_secs(3600))
                .unwrap()
                .is_none()
        );
        assert_eq!(s.events().unwrap().len(), 1);
        let request = BatchRequest {
            policy_id: "fixture".into(),
            batch_id: first.batch_id,
            members: first
                .members
                .iter()
                .map(|m| MemberRequest {
                    store_id: m.store_id.clone(),
                    operation_id: m.operation_id,
                    scheduled_at_unix_seconds: None,
                    byte_cap: Some(m.admitted_bytes),
                })
                .collect(),
        };
        assert!(!s.status(&request).unwrap().complete);
        s.tick(std::time::UNIX_EPOCH + Duration::from_secs(7200))
            .unwrap()
            .unwrap();
        settle(&s);
        let events = s.events().unwrap();
        assert_eq!(events.len(), 3);
        assert_eq!(events.last().unwrap().sink_id, "inert:owner");
        assert_eq!(events.last().unwrap().event.outcome.consecutive_failures, 2);
        assert_eq!(sink.keys.lock().unwrap().len(), 3);
        assert!(
            s.0.journal
                .lock()
                .unwrap()
                .deliveries
                .values()
                .all(|d| d.status == DeliveryStatus::Delivered && d.attempts == 1)
        );
    }
}

#[cfg(test)]
mod capacity_tests {
    use super::*;
    struct Space(u64);
    impl fs::RecoveryIo for Space {
        fn available_bytes(&self, _: &File) -> std::io::Result<u64> {
            Ok(self.0)
        }
    }
    fn policy(root: &std::path::Path, cap: u64) -> RecoveryPolicy {
        RecoveryPolicy {
            policy_id: "fixture".into(),
            store_id: "workspace".into(),
            destination: root.into(),
            source_map_sha256: "a".repeat(64),
            coordinator: true,
            members: vec![policy::Schedule {
                store_id: "remote".into(),
                endpoint: format!("unix://{}/remote.sock", root.display()),
                interval_seconds: 3600,
                prospective_bundle_bytes: cap,
                catalog_identity: "catalog:fixture".into(),
            }],
            limits: Some(policy::RecoveryLimits {
                retained_pin_bytes: 1 << 20,
                preparation_bytes: 2 << 20,
                memory_bytes: 32 << 20,
                disk_bytes: 64 << 20,
                image_bytes: 4096,
                metadata_bytes: 128 << 10,
                margin_bytes: 4096,
                total_seconds: 30,
            }),
            ..RecoveryPolicy::default()
        }
    }
    #[test]
    fn insufficient_space_and_overflow_fail_before_any_remote_connection() {
        for (space, cap) in [(0, 32 << 20), (u64::MAX, u64::MAX)] {
            let root = fixture_directory();
            std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
            let listener =
                std::os::unix::net::UnixListener::bind(root.path().join("remote.sock")).unwrap();
            listener.set_nonblocking(true).unwrap();
            let scheduler = RecoveryScheduler::start_shared_with_io(
                Arc::new(FerroStore::in_memory()),
                policy(root.path(), cap),
                None,
                Arc::new(Space(space)),
            )
            .unwrap();
            let request = BatchRequest {
                policy_id: "fixture".into(),
                batch_id: uuid::Uuid::now_v7(),
                members: vec![MemberRequest {
                    store_id: "remote".into(),
                    operation_id: uuid::Uuid::now_v7(),
                    scheduled_at_unix_seconds: None,
                    byte_cap: None,
                }],
            };
            assert!(scheduler.request_batch(&request).is_err());
            assert_eq!(
                listener.accept().unwrap_err().kind(),
                std::io::ErrorKind::WouldBlock
            );
            assert!(scheduler.0.journal.lock().unwrap().batches.is_empty());
        }
    }
}

#[cfg(test)]
mod event_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    pub(super) struct FailingSink(pub(super) AtomicUsize);
    impl RecoveryEventSink for FailingSink {
        fn emit(
            &self,
            _: &RecoveryEventEnvelope,
            _: &str,
            _: Instant,
        ) -> Result<(), RecoveryError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Err(RecoveryError::Refused(
                "/private/secret-path secret-document-content".into(),
            ))
        }
    }
    fn root() -> tempfile::TempDir {
        let root = fixture_directory();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        root
    }
    #[test]
    fn failed_sink_keeps_pending_across_restart_with_lifetime_retry_cap_and_safe_payload() {
        let root = root();
        let p = super::tests::policy(root.path());
        let proof = super::tests::proof(&p);
        let sink = Arc::new(FailingSink(AtomicUsize::new(0)));
        let scheduler = RecoveryScheduler::start_with_sink(
            FerroStore::in_memory(),
            p.clone(),
            Some(proof),
            sink.clone(),
        )
        .unwrap();
        scheduler
            .tick(std::time::UNIX_EPOCH + Duration::from_secs(3600))
            .unwrap();
        super::tests::settle(&scheduler);
        assert_eq!(sink.0.load(Ordering::SeqCst), 2);
        let events = scheduler.events().unwrap();
        let payload = serde_json::to_value(&events[0].event).unwrap();
        assert_eq!(
            payload
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect::<std::collections::BTreeSet<_>>(),
            ["operation_id", "catalog_identity", "outcome", "sizes"]
                .into_iter()
                .collect()
        );
        let encoded = serde_json::to_string(&events).unwrap();
        assert!(!encoded.contains("secret-path") && !encoded.contains("secret-document-content"));
        assert!(
            scheduler
                .0
                .journal
                .lock()
                .unwrap()
                .deliveries
                .values()
                .all(|d| d.status == DeliveryStatus::Pending && d.attempts == 2)
        );
        drop(scheduler);
        let restarted = RecoveryScheduler::start_with_sink(
            FerroStore::in_memory(),
            p.clone(),
            Some(super::tests::proof(&p)),
            sink.clone(),
        )
        .unwrap();
        assert_eq!(restarted.events().unwrap().len(), 1);
        assert_eq!(sink.0.load(Ordering::SeqCst), 2);
        assert!(
            restarted
                .tick(std::time::UNIX_EPOCH + Duration::from_secs(3600))
                .unwrap()
                .is_none()
        );
    }
    #[test]
    fn undelivered_full_outbox_never_blocks_next_scheduled_owner_attempt() {
        let root = root();
        let p = super::tests::policy(root.path());
        let sink = Arc::new(FailingSink(AtomicUsize::new(0)));
        let scheduler = RecoveryScheduler::start_with_sink(
            FerroStore::in_memory(),
            p.clone(),
            Some(super::tests::proof(&p)),
            sink,
        )
        .unwrap();
        for hour in 1..=34 {
            assert!(
                scheduler
                    .tick(std::time::UNIX_EPOCH + Duration::from_secs(hour * 3600))
                    .unwrap()
                    .is_some()
            );
            super::tests::settle(&scheduler);
        }
        let journal = scheduler.0.journal.lock().unwrap();
        assert_eq!(journal.batches.len(), 34);
        assert!(journal.outbox_full);
        assert_eq!(journal.events.len(), 32);
        assert!(
            journal
                .deliveries
                .values()
                .all(|d| d.status == DeliveryStatus::Pending)
        );
    }
    struct BlockedSink {
        entered: std::sync::mpsc::Sender<()>,
        release: Mutex<std::sync::mpsc::Receiver<()>>,
    }
    impl RecoveryEventSink for BlockedSink {
        fn emit(
            &self,
            _: &RecoveryEventEnvelope,
            _: &str,
            _: Instant,
        ) -> Result<(), RecoveryError> {
            self.entered.send(()).unwrap();
            self.release.lock().unwrap().recv().unwrap();
            Ok(())
        }
    }
    #[test]
    fn sink_timeout_is_bounded_and_never_claimed_delivered() {
        let (entered, entry) = std::sync::mpsc::channel();
        let (release, gate) = std::sync::mpsc::channel();
        let sink = Arc::new(BlockedSink {
            entered,
            release: Mutex::new(gate),
        });
        let event = RecoveryEventEnvelope {
            batch_id: uuid::Uuid::now_v7(),
            sink_id: "inert:fixture".into(),
            event: RecoveryEvent {
                operation_id: uuid::Uuid::now_v7(),
                catalog_identity: "catalog:fixture".into(),
                outcome: RecoveryEventOutcome {
                    code: RecoveryOutcomeCode::ScheduledOwnerFailure,
                    consecutive_failures: 1,
                },
                sizes: RecoverySizes::default(),
            },
        };
        let busy = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let flag = busy.clone();
        let (finished, completion) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let delivered = emit_bounded(
                sink,
                event,
                "fixture".into(),
                flag,
                Instant::now() + Duration::from_millis(20),
            );
            finished.send(delivered).unwrap();
        });
        entry.recv_timeout(Duration::from_secs(5)).unwrap();
        // Completion must precede releasing the callback, independently of
        // scheduler admission/backup/OS pacing. The timeout only prevents a
        // broken unbounded waiter from hanging the test process.
        let result = completion.recv_timeout(Duration::from_secs(5));
        release.send(()).unwrap();
        worker.join().unwrap();
        assert!(!result.unwrap());
        let deadline = Instant::now() + Duration::from_secs(5);
        while busy.load(Ordering::Acquire) {
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
    }
}

#[cfg(test)]
mod review_regressions {
    use super::*;
    struct NoSpace;
    impl fs::RecoveryIo for NoSpace {
        fn available_bytes(&self, _: &File) -> std::io::Result<u64> {
            Ok(0)
        }
    }

    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    #[derive(Default)]
    struct CountCapture(AtomicUsize);
    impl fs::RecoveryIo for CountCapture {
        fn before(&self, event: fs::RecoveryEvent) -> std::io::Result<()> {
            if event == fs::RecoveryEvent::Capture {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
            Ok(())
        }
    }
    fn native_owner(
        root: &std::path::Path,
    ) -> (RecoveryPolicy, RecoveryScheduler, Arc<CountCapture>) {
        native_owner_with(root, "project", None)
    }
    fn native_owner_with(
        root: &std::path::Path,
        store_id: &str,
        shared: Option<&std::path::Path>,
    ) -> (RecoveryPolicy, RecoveryScheduler, Arc<CountCapture>) {
        let source = root.join("source.fls");
        let owner = FerroStore::open(&source).unwrap();
        for n in 0..24 {
            owner
                .put_doc(
                    "fixture",
                    "one",
                    &serde_json::json!({"n":n,"body":"x".repeat(4096)}),
                )
                .unwrap();
        }
        let destination = shared.map_or_else(|| root.join("backups"), |p| p.to_path_buf());
        if shared.is_none() {
            std::fs::create_dir(&destination).unwrap();
        }
        std::fs::set_permissions(&destination, std::fs::Permissions::from_mode(0o700)).unwrap();
        let inventory = inventory::collect_inventory(&inventory::InventoryInputs {
            roots: vec![root.into()],
            bindings: vec![inventory::BindingInput::config(
                "project",
                source.clone(),
                Some("ferro".into()),
                "fixture",
            )],
            ..Default::default()
        })
        .unwrap();
        let identity = inventory
            .stores
            .into_iter()
            .find(|s| s.identity.canonical_path.as_ref() == Some(&source))
            .unwrap()
            .identity;
        let mut p = super::tests::policy(&destination);
        p.store_id = store_id.into();
        p.coordinator = false;
        p.members.clear();
        p.profile = ValidationProfile::new(ProfileKind::SmallEmpty, identity.catalog_key);
        p.limits.as_mut().unwrap().image_bytes = 16 << 10;
        let io = Arc::new(CountCapture::default());
        let scheduler = RecoveryScheduler::start_shared_with_runtime(
            Arc::new(owner),
            p.clone(),
            Some(super::tests::proof(&p)),
            io.clone(),
            Some(Arc::new(super::tests::InertSink::default())),
        )
        .unwrap();
        (p, scheduler, io)
    }
    fn read_value(stream: &mut std::os::unix::net::UnixStream) -> serde_json::Value {
        let mut size = [0; 4];
        stream.read_exact(&mut size).unwrap();
        let n = u32::from_be_bytes(size) as usize;
        assert!(n <= 1 << 20);
        let mut bytes = vec![0; n];
        stream.read_exact(&mut bytes).unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }
    fn write_value(stream: &mut std::os::unix::net::UnixStream, value: &serde_json::Value) {
        let bytes = serde_json::to_vec(value).unwrap();
        stream
            .write_all(&(bytes.len() as u32).to_be_bytes())
            .unwrap();
        stream.write_all(&bytes).unwrap();
    }
    struct OwnerFixture {
        online: Arc<AtomicBool>,
        wrong_batch: Arc<AtomicBool>,
        stop: Arc<AtomicBool>,
        thread: Option<std::thread::JoinHandle<()>>,
    }
    impl Drop for OwnerFixture {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            if let Some(t) = self.thread.take() {
                t.join().unwrap();
            }
        }
    }
    fn owner_endpoint(
        root: &std::path::Path,
        owner: RecoveryScheduler,
        drop_reply: bool,
    ) -> (String, OwnerFixture) {
        let socket = root.join("owner.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let online = Arc::new(AtomicBool::new(true));
        let stop = Arc::new(AtomicBool::new(false));
        let available = online.clone();
        let wrong_batch = Arc::new(AtomicBool::new(false));
        let malformed = wrong_batch.clone();
        let done = stop.clone();
        let thread = std::thread::spawn(move || {
            let mut drop_reply = drop_reply;
            while !done.load(Ordering::Acquire) {
                let (mut stream, _) = match listener.accept() {
                    Ok(s) => s,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(1));
                        continue;
                    }
                    Err(e) => panic!("{e}"),
                };
                if !available.load(Ordering::Acquire) {
                    continue;
                }
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                write_value(
                    &mut stream,
                    &serde_json::json!({"auth":"ok","scope":"admin"}),
                );
                let request = read_value(&mut stream);
                let batch: BatchRequest =
                    serde_json::from_value(request["params"].clone()).unwrap();
                let result = if request["method"] == "recovery_start" {
                    owner.request_batch(&batch)
                } else {
                    owner.status(&batch)
                };
                if drop_reply && request["method"] == "recovery_start" {
                    drop_reply = false;
                    continue;
                }
                let response = match result {
                    Ok(mut report) => {
                        if malformed.load(Ordering::Acquire) {
                            report.batch_id = uuid::Uuid::now_v7();
                            for member in &mut report.members {
                                member.status = MemberStatus::Failed;
                            }
                            report.complete = false;
                        }
                        serde_json::json!({"id":request["id"],"ok":true,"result":report})
                    }
                    Err(_) => serde_json::json!({"id":request["id"],"ok":false}),
                };
                write_value(&mut stream, &response);
            }
        });
        (
            format!("unix://{}", socket.display()),
            OwnerFixture {
                online,
                wrong_batch,
                stop,
                thread: Some(thread),
            },
        )
    }
    fn coordinator(p: &RecoveryPolicy, endpoint: String) -> RecoveryScheduler {
        let mut coordinator = p.clone();
        coordinator.store_id = "coordinator".into();
        coordinator.coordinator = true;
        coordinator.members = vec![policy::Schedule {
            store_id: "project".into(),
            endpoint,
            interval_seconds: 3600,
            prospective_bundle_bytes: 32 << 20,
            catalog_identity: p.profile.catalog_identity.clone(),
        }];
        RecoveryScheduler::start_with_sink(
            FerroStore::in_memory(),
            coordinator.clone(),
            Some(super::tests::proof(&coordinator)),
            Arc::new(super::tests::InertSink::default()),
        )
        .unwrap()
    }
    fn await_background(s: &RecoveryScheduler, batch: uuid::Uuid, hour: u64) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            s.tick(std::time::UNIX_EPOCH + Duration::from_secs(hour * 3600))
                .unwrap();
            super::tests::settle(s);
            if s.0
                .journal
                .lock()
                .unwrap()
                .batches
                .get(&batch)
                .unwrap()
                .complete
            {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "batch never completed through timer ticks"
            );
            std::thread::yield_now();
        }
    }
    #[test]
    fn timer_ticks_complete_two_real_remote_batches_without_status_polling() {
        let root = fixture_directory();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let (p, owner, captures) = native_owner(root.path());
        let (endpoint, _fixture) = owner_endpoint(root.path(), owner, false);
        let s = coordinator(&p, endpoint);
        for hour in [1, 2] {
            let first = s
                .tick(std::time::UNIX_EPOCH + Duration::from_secs(hour * 3600))
                .unwrap()
                .unwrap();
            super::tests::settle(&s);
            await_background(&s, first.batch_id, hour);
            assert!(
                p.destination
                    .join(format!(".batch-{}.json", first.batch_id))
                    .exists()
            );
        }
        assert_eq!(captures.0.load(Ordering::SeqCst), 2);
    }
    // A valid frame and exact member IDs are insufficient when the enclosing
    // batch ID differs: accepting its terminal status would free pending space.
    #[test]
    fn wrong_batch_reply_keeps_background_operation_unknown_and_reserved() {
        let root = fixture_directory();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let (p, owner, captures) = native_owner(root.path());
        let (endpoint, fixture) = owner_endpoint(root.path(), owner.clone(), false);
        fixture.wrong_batch.store(true, Ordering::Release);
        let s = coordinator(&p, endpoint.clone());
        let first = s
            .tick(std::time::UNIX_EPOCH + Duration::from_secs(3600))
            .unwrap()
            .unwrap();
        super::tests::settle(&s);
        super::tests::settle(&owner);
        let request = BatchRequest {
            policy_id: p.policy_id.clone(),
            batch_id: first.batch_id,
            members: vec![MemberRequest {
                store_id: "project".into(),
                operation_id: first.members[0].operation_id,
                scheduled_at_unix_seconds: None,
                byte_cap: Some(first.members[0].admitted_bytes),
            }],
        };
        let result = control::call_until(
            &endpoint,
            "recovery_status",
            &request,
            Instant::now() + Duration::from_secs(2),
        );
        assert!(
            matches!(result, Err(control::OwnerCallError::OutcomeUnknown)),
            "wrong-batch reply must be uncertain: {result:?}"
        );
        let pending = s.0.journal.lock().unwrap().batches[&first.batch_id].clone();
        assert_eq!(pending.members[0].status, MemberStatus::OutcomeUnknown);
        assert_eq!(
            pending.members[0].operation_id,
            first.members[0].operation_id
        );
        let reservation = p
            .destination
            .join(".reservations")
            .join(format!("{}.json", first.batch_id));
        assert!(reservation.exists());
        s.tick(std::time::UNIX_EPOCH + Duration::from_secs(3600))
            .unwrap();
        super::tests::settle(&s);
        assert!(reservation.exists());
        assert_eq!(
            s.0.journal.lock().unwrap().batches[&first.batch_id].members[0].status,
            MemberStatus::OutcomeUnknown
        );
        fixture.wrong_batch.store(false, Ordering::Release);
        await_background(&s, first.batch_id, 1);
        assert!(!reservation.exists());
        assert_eq!(
            s.0.journal.lock().unwrap().batches[&first.batch_id].members[0].operation_id,
            first.members[0].operation_id
        );
        assert_eq!(captures.0.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn wrong_batch_reply_keeps_manual_status_running_and_reserved() {
        let root = fixture_directory();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let (p, owner, captures) = native_owner(root.path());
        let (endpoint, fixture) = owner_endpoint(root.path(), owner.clone(), false);
        let s = coordinator(&p, endpoint.clone());
        let first = s
            .tick(std::time::UNIX_EPOCH + Duration::from_secs(3600))
            .unwrap()
            .unwrap();
        super::tests::settle(&s);
        super::tests::settle(&owner);
        let request = BatchRequest {
            policy_id: p.policy_id.clone(),
            batch_id: first.batch_id,
            members: vec![MemberRequest {
                store_id: "project".into(),
                operation_id: first.members[0].operation_id,
                scheduled_at_unix_seconds: None,
                byte_cap: Some(first.members[0].admitted_bytes),
            }],
        };
        fixture.wrong_batch.store(true, Ordering::Release);
        let result = control::call(&endpoint, "recovery_status", &request);
        assert!(
            result.is_err(),
            "public call accepted wrong-batch reply: {result:?}"
        );
        let pending = s.status(&request).unwrap();
        assert_eq!(pending.batch_id, first.batch_id);
        assert_eq!(
            pending.members[0].operation_id,
            first.members[0].operation_id
        );
        assert_eq!(pending.members[0].status, MemberStatus::Running);
        let reservation = p
            .destination
            .join(".reservations")
            .join(format!("{}.json", first.batch_id));
        assert!(reservation.exists());
        fixture.wrong_batch.store(false, Ordering::Release);
        let completed = s.status(&request).unwrap();
        assert!(completed.complete);
        assert_eq!(
            completed.members[0].operation_id,
            first.members[0].operation_id
        );
        assert!(!reservation.exists());
        assert_eq!(captures.0.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn lost_admitted_reply_and_unavailable_restart_keep_same_operation_reserved() {
        let root = fixture_directory();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let (p, owner, captures) = native_owner(root.path());
        let (endpoint, fixture) = owner_endpoint(root.path(), owner, true);
        let s = coordinator(&p, endpoint.clone());
        let first = s
            .tick(std::time::UNIX_EPOCH + Duration::from_secs(3600))
            .unwrap()
            .unwrap();
        super::tests::settle(&s);
        assert_eq!(
            s.0.journal.lock().unwrap().batches[&first.batch_id].members[0].status,
            MemberStatus::OutcomeUnknown
        );
        let reservation = p
            .destination
            .join(".reservations")
            .join(format!("{}.json", first.batch_id));
        assert!(reservation.exists());
        fixture.online.store(false, Ordering::Release);
        drop(s);
        let restarted = coordinator(&p, endpoint);
        super::tests::settle(&restarted);
        assert_eq!(
            restarted.0.journal.lock().unwrap().batches[&first.batch_id].members[0].status,
            MemberStatus::OutcomeUnknown
        );
        assert!(
            restarted
                .tick(std::time::UNIX_EPOCH + Duration::from_secs(7200))
                .unwrap()
                .is_none()
        );
        super::tests::settle(&restarted);
        assert_eq!(restarted.0.journal.lock().unwrap().batches.len(), 1);
        assert!(reservation.exists());
        fixture.online.store(true, Ordering::Release);
        await_background(&restarted, first.batch_id, 1);
        assert_eq!(
            restarted.0.journal.lock().unwrap().batches[&first.batch_id].members[0].operation_id,
            first.members[0].operation_id
        );
        assert_eq!(captures.0.load(Ordering::SeqCst), 1);
        assert!(!reservation.exists());
    }

    #[test]
    fn mixed_member_restart_releases_creators_whole_reservation_after_subset_reconcile() {
        let first_root = fixture_directory();
        std::fs::set_permissions(first_root.path(), std::fs::Permissions::from_mode(0o700))
            .unwrap();
        let second_root = fixture_directory();
        std::fs::set_permissions(second_root.path(), std::fs::Permissions::from_mode(0o700))
            .unwrap();
        let (p, first_owner, first_captures) = native_owner(first_root.path());
        let (second_policy, second_owner, second_captures) =
            native_owner_with(second_root.path(), "second", Some(&p.destination));
        let (first_endpoint, _first_fixture) =
            owner_endpoint(first_root.path(), first_owner.clone(), false);
        let (second_endpoint, second_fixture) =
            owner_endpoint(second_root.path(), second_owner.clone(), true);
        let mut cp = p.clone();
        cp.store_id = "coordinator".into();
        cp.coordinator = true;
        cp.members = vec![
            policy::Schedule {
                store_id: "project".into(),
                endpoint: first_endpoint,
                interval_seconds: 3600,
                prospective_bundle_bytes: 32 << 20,
                catalog_identity: p.profile.catalog_identity.clone(),
            },
            policy::Schedule {
                store_id: "second".into(),
                endpoint: second_endpoint,
                interval_seconds: 3600,
                prospective_bundle_bytes: 32 << 20,
                catalog_identity: second_policy.profile.catalog_identity,
            },
        ];
        let start = || {
            RecoveryScheduler::start_with_sink(
                FerroStore::in_memory(),
                cp.clone(),
                Some(super::tests::proof(&cp)),
                Arc::new(super::tests::InertSink::default()),
            )
            .unwrap()
        };
        let s = start();
        let first = s
            .tick(std::time::UNIX_EPOCH + Duration::from_secs(3600))
            .unwrap()
            .unwrap();
        super::tests::settle(&s);
        // Wait on the actual owners' workers before selecting the mixed
        // Durable/unknown restart cut; the start RPC intentionally returns
        // Running and never promises capture completion.
        super::tests::settle(&first_owner);
        super::tests::settle(&second_owner);
        second_fixture.online.store(false, Ordering::Release);
        s.tick(std::time::UNIX_EPOCH + Duration::from_secs(3600))
            .unwrap();
        super::tests::settle(&s);
        let snapshot = s.0.journal.lock().unwrap().batches[&first.batch_id].clone();
        assert_eq!(snapshot.members[0].status, MemberStatus::Durable);
        assert_eq!(snapshot.members[1].status, MemberStatus::OutcomeUnknown);
        let reservation = p
            .destination
            .join(".reservations")
            .join(format!("{}.json", first.batch_id));
        assert!(reservation.exists());
        drop(s);
        let restarted = start();
        super::tests::settle(&restarted);
        assert!(reservation.exists());
        assert_eq!(
            restarted.0.journal.lock().unwrap().batches[&first.batch_id].members[0].status,
            MemberStatus::Durable
        );
        second_fixture.online.store(true, Ordering::Release);
        await_background(&restarted, first.batch_id, 1);
        assert!(!reservation.exists());
        assert_eq!(first_captures.0.load(Ordering::SeqCst), 1);
        assert_eq!(second_captures.0.load(Ordering::SeqCst), 1);
    }
    #[test]
    fn verified_metadata_replacement_cannot_supply_report_or_block_on_fifo() {
        let root = fixture_directory();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let (p, s, _) = native_owner(root.path());
        let batch = uuid::Uuid::now_v7();
        let operation = uuid::Uuid::now_v7();
        assert_eq!(
            s.local(batch, operation, false).unwrap().status,
            MemberStatus::Durable
        );
        let path = p.destination.join(operation.to_string());
        let manifest = path.join("manifest.json");
        let mut changed: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&manifest).unwrap()).unwrap();
        changed["published_sequence"] = serde_json::json!(999999);
        let replacement = path.join("replacement.json");
        std::fs::write(&replacement, serde_json::to_vec(&changed).unwrap()).unwrap();
        std::fs::set_permissions(&replacement, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::rename(&replacement, &manifest).unwrap();
        let limits = s.accounting_limits(operation).unwrap();
        assert!(
            s.report_bundle(
                batch,
                operation,
                &backup::VerifiedBundle { path: path.clone() },
                &limits
            )
            .is_err()
        );
        std::fs::remove_file(&manifest).unwrap();
        use std::os::unix::ffi::OsStrExt;
        let name = std::ffi::CString::new(manifest.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let (finished, completion) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            finished
                .send(
                    s.report_bundle(batch, operation, &backup::VerifiedBundle { path }, &limits)
                        .is_err(),
                )
                .unwrap();
        });
        let result = completion.recv_timeout(Duration::from_secs(2));
        // Open a nonblocking writer only on regression, so a removed O_NONBLOCK
        // cannot leave a stuck worker after the assertion reports the defect.
        if result.is_err() {
            let fd = unsafe { libc::open(name.as_ptr(), libc::O_WRONLY | libc::O_NONBLOCK) };
            if fd >= 0 {
                unsafe { libc::close(fd) };
            }
        }
        worker.join().unwrap();
        assert!(result.unwrap());
    }
    #[test]
    fn encoded_history_refuses_safely_preserves_bindings_and_reopens() {
        let root = fixture_directory();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let p = super::tests::policy(root.path());
        let s = RecoveryScheduler::start_shared_with_runtime(
            Arc::new(FerroStore::in_memory()),
            p.clone(),
            Some(super::tests::proof(&p)),
            Arc::new(NoSpace),
            Some(Arc::new(super::tests::InertSink::default())),
        )
        .unwrap();
        let report = s
            .tick(std::time::UNIX_EPOCH + Duration::from_secs(3600))
            .unwrap()
            .unwrap();
        let replay = BatchRequest {
            policy_id: p.policy_id.clone(),
            batch_id: report.batch_id,
            members: report
                .members
                .iter()
                .map(|m| MemberRequest {
                    store_id: m.store_id.clone(),
                    operation_id: m.operation_id,
                    scheduled_at_unix_seconds: None,
                    byte_cap: None,
                })
                .collect(),
        };
        {
            let mut journal = s.0.journal.lock().unwrap();
            journal.batches.get_mut(&report.batch_id).unwrap().members[0].reason =
                Some("x".repeat(JOURNAL_BYTES - 20000));
            s.save(&mut journal).unwrap();
            journal.batches.get_mut(&report.batch_id).unwrap().members[0].reason =
                Some("x".repeat(JOURNAL_BYTES + 1));
            assert!(s.save(&mut journal).is_err());
            assert!(serde_json::to_vec(&*journal).unwrap().len() <= JOURNAL_BYTES);
        }
        for hour in 2..=20 {
            s.tick(std::time::UNIX_EPOCH + Duration::from_secs(hour * 3600))
                .unwrap();
        }
        assert_eq!(
            s.request_batch(&replay).unwrap().members[0].operation_id,
            replay.members[0].operation_id
        );
        let mut changed = replay.clone();
        changed.members[0].operation_id = uuid::Uuid::now_v7();
        assert!(s.request_batch(&changed).is_err());
        let journal = s.0.journal.lock().unwrap();
        assert_eq!(journal.batches.len(), 1);
        assert_eq!(journal.consecutive_failures, 20);
        assert!(journal.history_full);
        drop(journal);
        drop(s);
        let reopened = RecoveryScheduler::start_shared_with_runtime(
            Arc::new(FerroStore::in_memory()),
            p.clone(),
            Some(super::tests::proof(&p)),
            Arc::new(NoSpace),
            Some(Arc::new(super::tests::InertSink::default())),
        )
        .unwrap();
        assert_eq!(reopened.0.journal.lock().unwrap().consecutive_failures, 20);
        assert_eq!(
            reopened.request_batch(&replay).unwrap().members[0].operation_id,
            replay.members[0].operation_id
        );
    }
    #[test]
    fn unordered_fresh_ids_are_accepted_and_save_failure_stops_further_admission() {
        let root = fixture_directory();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let (p, s, captures) = native_owner(root.path());
        let request = BatchRequest {
            policy_id: p.policy_id.clone(),
            batch_id: uuid::Uuid::from_u128(17),
            members: vec![MemberRequest {
                store_id: p.store_id.clone(),
                operation_id: uuid::Uuid::from_u128(3),
                scheduled_at_unix_seconds: None,
                byte_cap: None,
            }],
        };
        s.request_batch(&request).unwrap();
        super::tests::settle(&s);
        assert!(s.status(&request).unwrap().complete);
        assert_eq!(captures.0.load(Ordering::SeqCst), 1);
        assert!(
            !p.destination
                .join(".reservations")
                .join(format!("{}.json", request.batch_id))
                .exists()
        );
        let name = inventory::metadata_digest(p.store_id.as_bytes());
        let temp = p.destination.join(format!(".scheduler-{name}.tmp"));
        std::os::unix::fs::symlink("not-a-journal", &temp).unwrap();
        let mut next = request.clone();
        next.batch_id = uuid::Uuid::now_v7();
        next.members[0].operation_id = uuid::Uuid::now_v7();
        assert!(s.request_batch(&next).is_err());
        assert!(s.0.persistence_failed.load(Ordering::Acquire));
        assert!(s.request_batch(&next).is_err());
        assert_eq!(captures.0.load(Ordering::SeqCst), 1);
        assert!(
            p.destination
                .join(".reservations")
                .join(format!("{}.json", next.batch_id))
                .exists()
        );
        assert!(
            s.0.journal.lock().unwrap().batches[&next.batch_id]
                .members
                .iter()
                .all(|m| m.status == MemberStatus::Running)
        );
    }
    #[test]
    fn full_history_keeps_completed_operation_and_member_set_binding() {
        let root = fixture_directory();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let (p, s, captures) = native_owner(root.path());
        let request = BatchRequest {
            policy_id: p.policy_id.clone(),
            batch_id: uuid::Uuid::from_u128(17),
            members: vec![MemberRequest {
                store_id: p.store_id.clone(),
                operation_id: uuid::Uuid::from_u128(3),
                scheduled_at_unix_seconds: None,
                byte_cap: None,
            }],
        };
        s.request_batch(&request).unwrap();
        super::tests::settle(&s);
        let original = s.status(&request).unwrap();
        assert!(original.complete);
        {
            let mut journal = s.0.journal.lock().unwrap();
            for n in 0..1023 {
                let mut row = original.clone();
                row.batch_id = uuid::Uuid::from_u128(100 + n);
                row.complete = false;
                row.members[0].operation_id = uuid::Uuid::from_u128(10000 + n);
                row.members[0].status = MemberStatus::Failed;
                journal.batches.insert(row.batch_id, row);
            }
            s.save(&mut journal).unwrap();
        }
        assert!(s.request_batch(&request).unwrap().complete);
        let mut altered = request.clone();
        altered.members[0].operation_id = uuid::Uuid::now_v7();
        assert!(s.request_batch(&altered).is_err());
        altered.batch_id = uuid::Uuid::now_v7();
        assert!(s.request_batch(&altered).is_err());
        assert_eq!(captures.0.load(Ordering::SeqCst), 1);
    }
    #[test]
    fn refused_slots_bound_all_history_and_preserve_pending_outbox_on_reopen() {
        let root = fixture_directory();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let p = super::tests::policy(root.path());
        let s = RecoveryScheduler::start_shared_with_runtime(
            Arc::new(FerroStore::in_memory()),
            p.clone(),
            Some(super::tests::proof(&p)),
            Arc::new(NoSpace),
            Some(Arc::new(super::event_tests::FailingSink(
                std::sync::atomic::AtomicUsize::new(0),
            ))),
        )
        .unwrap();
        for hour in 1..=1100 {
            s.tick(std::time::UNIX_EPOCH + Duration::from_secs(hour * 3600))
                .unwrap();
        }
        let events = s.events().unwrap();
        assert_eq!(events.len(), 32);
        {
            let journal = s.0.journal.lock().unwrap();
            assert!(
                journal.batches.len() <= 1024,
                "{} batches",
                journal.batches.len()
            );
            assert!(journal.scheduled.len() <= 1024);
            assert!(journal.notified.len() <= 1024);
            assert_eq!(journal.consecutive_failures, 1100);
            assert!(serde_json::to_vec(&*journal).unwrap().len() <= 1_048_576);
        }
        drop(s);
        let restored = RecoveryScheduler::start_shared_with_runtime(
            Arc::new(FerroStore::in_memory()),
            p.clone(),
            Some(super::tests::proof(&p)),
            Arc::new(NoSpace),
            Some(Arc::new(super::event_tests::FailingSink(
                std::sync::atomic::AtomicUsize::new(0),
            ))),
        )
        .unwrap();
        assert_eq!(
            serde_json::to_value(restored.events().unwrap()).unwrap(),
            serde_json::to_value(events).unwrap()
        );
        assert_eq!(
            restored.0.journal.lock().unwrap().consecutive_failures,
            1100
        );
        assert!(
            restored
                .tick(std::time::UNIX_EPOCH + Duration::from_secs(1100 * 3600))
                .unwrap()
                .is_none()
        );
    }
}
