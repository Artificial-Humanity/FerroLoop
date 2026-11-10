//! Immutable physical captures. The capture cut is a published superblock,
//! not proof of another opener's fsync result (issue #456).
use super::layout::{self, HEADER_BYTES, SLOT_LEN, SlotRead, Superblock};
use super::{ContainerIo, Descriptor, DocState, FerroStore, HeldFlock, Inner};
use crate::recovery::{CancelToken, CaptureBudget, OperationLimits, RecoveryError};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io;
use std::os::unix::fs::FileExt;
use std::sync::{Arc, Mutex, MutexGuard, TryLockError};
use std::time::{Duration, Instant};
use uuid::Uuid;

/// Pre-P1 locks serialize publishers but do not share per-handle health.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExternalPublishStatus {
    /// An external writer may have reported a failed final sync for this S.
    ExclusionUnprovenBeforeSingleOwnership,
}

/// The complete image's intrinsic metadata. No intrinsic lineage is presently
/// recorded in this engine; absence is explicit, never a synthetic identifier.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageDescriptor {
    pub bytes: u64,
    pub sequence: u64,
    pub container_format: u32,
    pub snapshot_format: u32,
    pub engine: u32,
    pub incompatible_features: u64,
    pub schema: Option<u64>,
    pub lineage: Option<String>,
    pub external_publish_status: ExternalPublishStatus,
    /// Memory-tier MQ and pending adapter deliveries are not container state.
    pub exclusions: Vec<CaptureExclusion>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureExclusion {
    EphemeralMq,
    PendingAdapterDeliveries,
}

/// A private destination supplied by the recovery coordinator. Implementations
/// must write the entire slice or return an error, and must not alias the source.
/// Calls are bounded in size; a blocking filesystem call is not preemptible.
pub trait RecoveryImageWriter {
    fn write_at(&mut self, offset: u64, bytes: &[u8]) -> io::Result<()>;
    fn set_len(&mut self, len: u64) -> io::Result<()>;
    fn sync_all(&mut self) -> io::Result<()>;
}
impl RecoveryImageWriter for std::fs::File {
    fn write_at(&mut self, offset: u64, bytes: &[u8]) -> io::Result<()> {
        self.write_all_at(bytes, offset)
    }
    fn set_len(&mut self, len: u64) -> io::Result<()> {
        std::fs::File::set_len(self, len)
    }
    fn sync_all(&mut self) -> io::Result<()> {
        std::fs::File::sync_all(self)
    }
}

/// One immutable state and its format pin. No source handle, pathname, mutable
/// index or I/O description is retained. All mutations use Arc::make_mut;
/// vector graphs and column groups own their data (see state.rs).
pub struct CapturedStore {
    state: Arc<DocState>,
    publication: Publication,
    retained: u64,
    preparation: u64,
    writer_delay: Duration,
    source_file_identity: (u64, u64),
    captured_at_unix_ms: u128,
}
impl CapturedStore {
    pub fn sequence(&self) -> u64 {
        self.state.last_seq
    }
    /// Conservative retained heap bytes, including map/Arc overhead and capacity.
    pub fn retained_bytes(&self) -> u64 {
        self.retained
    }
    /// Peak preparation admission charge, including the independent pin,
    /// codec capacity, bounded scratch and a 2x model safety factor. This is
    /// an upper-bound reservation, not observed allocator usage or process RSS.
    pub fn preparation_bytes(&self) -> u64 {
        self.preparation
    }
    /// Total time this capture held any commit barrier, including stale attempts.
    pub fn writer_delay(&self) -> Duration {
        self.writer_delay
    }
    pub fn external_publish_status(&self) -> ExternalPublishStatus {
        ExternalPublishStatus::ExclusionUnprovenBeforeSingleOwnership
    }
    pub fn write_image(
        &self,
        target: &mut dyn RecoveryImageWriter,
        limits: &OperationLimits,
    ) -> Result<ImageDescriptor, RecoveryError> {
        let clock = RealClock;
        let check = Check {
            id: limits.operation_id,
            deadline: limits.deadline,
            cancel: &limits.cancel,
            clock: &clock,
        };
        check.check()?;
        // Existing codecs allocate a growing body plus one escaped JSON value,
        // and vector validation scratch. Escaping <=6x input, Vec growth <=2x;
        // 32x the conservative owned-state charge covers their simultaneous peak.
        let working = mul(self.retained, 32, &check, "encoding memory")?;
        quota(working, limits.memory_byte_limit, &check, "encoding memory")?;
        let blob =
            layout::encode_snapshot_for_recovery(&self.state, &mut || codec_progress(&check))
                .map_err(|err| decode_error(err, &check))?;
        check.check()?;
        let bytes = add(HEADER_BYTES, blob.len() as u64, &check, "image bytes")?;
        quota(bytes, limits.byte_limit, &check, "image bytes")?;
        // Build everything that may fail source validation before destination I/O.
        let schema = super::doc::schema_version_in(|c, id| self.state.docs.get(c, id).cloned())
            .map_err(|_| check.verify())?;
        let sb = Superblock {
            epoch: 1,
            snapshot_offset: HEADER_BYTES,
            snapshot_length: blob.len() as u64,
            snapshot_seq: self.sequence(),
            wal_offset: bytes,
            wal_length: 0,
        };
        let slot = layout::encode_superblock_as(
            &sb,
            self.publication.format,
            self.publication.engine,
            self.publication.features,
        );
        check.check()?;
        target.set_len(bytes).map_err(|_| check.destination())?;
        check.check()?;
        // Overwrite both header slots, including a reused destination's second slot.
        target.write_at(0, &slot).map_err(|_| check.destination())?;
        check.check()?;
        target
            .write_at(SLOT_LEN as u64, &[0u8; SLOT_LEN])
            .map_err(|_| check.destination())?;
        for (chunk, data) in blob.chunks(64 * 1024).enumerate() {
            check.check()?;
            let offset = add(
                HEADER_BYTES,
                mul(chunk as u64, 64 * 1024, &check, "image offset")?,
                &check,
                "image offset",
            )?;
            target
                .write_at(offset, data)
                .map_err(|_| check.destination())?;
        }
        check.check()?;
        target.sync_all().map_err(|_| check.destination())?;
        check.check()?;
        Ok(ImageDescriptor {
            bytes,
            sequence: self.sequence(),
            container_format: self.publication.format,
            snapshot_format: if !self.state.vectors.is_empty() {
                layout::SNAPSHOT_VERSION_VECTOR
            } else if !self.state.tables.is_empty() {
                layout::SNAPSHOT_VERSION_COLUMNAR
            } else {
                layout::SNAPSHOT_VERSION_DOCS
            },
            engine: self.publication.engine,
            incompatible_features: self.publication.features,
            schema,
            lineage: None,
            external_publish_status: self.external_publish_status(),
            exclusions: vec![
                CaptureExclusion::EphemeralMq,
                CaptureExclusion::PendingAdapterDeliveries,
            ],
        })
    }
}

trait Clock {
    fn now(&self) -> Instant;
    fn wait(&self);
}
struct RealClock;
impl Clock for RealClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
    fn wait(&self) {
        std::thread::sleep(Duration::from_millis(1));
    }
}
struct Check<'a> {
    id: Uuid,
    deadline: Instant,
    cancel: &'a CancelToken,
    clock: &'a dyn Clock,
}
impl Check<'_> {
    fn check(&self) -> Result<(), RecoveryError> {
        if self.cancel.is_cancelled() {
            return Err(RecoveryError::Cancelled {
                operation_id: self.id,
            });
        }
        if self.clock.now() >= self.deadline {
            return Err(self.expired());
        }
        Ok(())
    }
    fn expired(&self) -> RecoveryError {
        RecoveryError::Deadline {
            operation_id: self.id,
        }
    }
    fn verify(&self) -> RecoveryError {
        RecoveryError::Verification {
            operation_id: self.id,
        }
    }
    fn destination(&self) -> RecoveryError {
        RecoveryError::Destination {
            operation_id: self.id,
        }
    }
    fn unhealthy(&self) -> RecoveryError {
        RecoveryError::UnhealthySource {
            operation_id: self.id,
        }
    }
    fn quota(&self, resource: &'static str) -> RecoveryError {
        RecoveryError::Quota {
            operation_id: self.id,
            resource,
        }
    }
}
fn add(a: u64, b: u64, c: &Check<'_>, resource: &'static str) -> Result<u64, RecoveryError> {
    a.checked_add(b).ok_or_else(|| c.quota(resource))
}
fn mul(a: u64, b: u64, c: &Check<'_>, resource: &'static str) -> Result<u64, RecoveryError> {
    a.checked_mul(b).ok_or_else(|| c.quota(resource))
}
fn quota(n: u64, limit: u64, c: &Check<'_>, resource: &'static str) -> Result<(), RecoveryError> {
    if n > limit {
        Err(c.quota(resource))
    } else {
        Ok(())
    }
}
fn mutex<'a, T>(m: &'a Mutex<T>, c: &Check<'_>) -> Result<MutexGuard<'a, T>, RecoveryError> {
    loop {
        c.check()?;
        match m.try_lock() {
            Ok(g) => return Ok(g),
            Err(TryLockError::Poisoned(_)) => return Err(c.unhealthy()),
            Err(TryLockError::WouldBlock) => c.clock.wait(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Publication {
    sb: Superblock,
    format: u32,
    engine: u32,
    features: u64,
}
fn publication(file: &dyn ContainerIo, c: &Check<'_>) -> Result<Publication, RecoveryError> {
    c.check()?;
    let len = file.len().map_err(|_| c.verify())?;
    let mut best: Option<Publication> = None;
    for index in 0..2 {
        c.check()?;
        if len < (index + 1) * SLOT_LEN as u64 {
            continue;
        }
        let mut buf = [0u8; SLOT_LEN];
        file.read_at(&mut buf, index * SLOT_LEN as u64)
            .map_err(|_| c.verify())?;
        match layout::parse_superblock(&buf) {
            SlotRead::Valid(sb) => {
                let Some((format, Some((engine, features)))) = layout::slot_header(&buf) else {
                    return Err(c.verify());
                };
                if best.is_some_and(|p| p.sb.epoch == sb.epoch) {
                    return Err(c.verify());
                }
                if best.is_none_or(|p| p.sb.epoch < sb.epoch) {
                    best = Some(Publication {
                        sb,
                        format,
                        engine,
                        features,
                    });
                }
            }
            SlotRead::Invalid => {}
            SlotRead::Newer { .. } => {
                return Err(RecoveryError::Unsupported {
                    operation_id: c.id,
                    reason: "container format unsupported",
                });
            }
        }
    }
    c.check()?;
    best.ok_or_else(|| c.verify())
}

impl FerroStore {
    /// Capture a refreshed immutable published state. This does not create/open
    /// a fallback store, checkpoint, flush MQ, sync the source, or update its cache.
    pub fn capture_recovery(
        &self,
        budget: &CaptureBudget,
        cancel: &CancelToken,
    ) -> Result<CapturedStore, RecoveryError> {
        self.capture_recovery_with_clock(budget, cancel, &RealClock)
    }
    fn capture_recovery_with_clock(
        &self,
        budget: &CaptureBudget,
        cancel: &CancelToken,
        clock: &dyn Clock,
    ) -> Result<CapturedStore, RecoveryError> {
        let check = Check {
            id: Uuid::now_v7(),
            deadline: budget.deadline,
            cancel,
            clock,
        };
        check.check()?;
        if budget.writer_delay_limit.is_zero()
            || budget.writer_delay_limit > Duration::from_secs(1)
            || budget.pin_byte_limit == 0
            || budget.preparation_byte_limit == 0
        {
            return Err(RecoveryError::RefusedInput {
                operation_id: check.id,
                reason: "explicit bounded capture limits required",
            });
        }
        let Inner::Disk(disk) = &self.inner else {
            return Err(RecoveryError::Unsupported {
                operation_id: check.id,
                reason: "physical owner handle required",
            });
        };
        // The usual reentry check takes a blocking mutex; use bounded admission.
        if mutex(&self.body_threads, &check)?.contains(&std::thread::current().id()) {
            return Err(RecoveryError::Unsupported {
                operation_id: check.id,
                reason: "capture from transaction body refused",
            });
        }
        let mut delay = Duration::ZERO;
        let mut preparation_peak = 0;
        // Initial attempt plus at most three stale-preparation retries.
        for _ in 0..=3 {
            check.check()?;
            if disk.poison.is_set() {
                return Err(check.unhealthy());
            }
            let file = disk.fs.open_ro(&disk.path).map_err(|_| check.verify())?;
            let descriptor = Descriptor::new(file, &disk.path).map_err(|_| check.verify())?;
            let published = publication(&*descriptor.file, &check)?;
            // Never take an uncharged Arc of the live cache: while accounting
            // it, a concurrent writer could COW an arbitrarily large collection.
            // Independent preparation bounds every allocation before decoding,
            // then moves the complete owned state into the immutable pin.
            let (state, preparation) = prepare(&*descriptor.file, &published.sb, budget, &check)?;
            preparation_peak = preparation_peak.max(preparation);
            let state = Arc::new(state);
            let retained = retained_bytes(
                &state,
                budget.pin_byte_limit.min(budget.preparation_byte_limit),
                &check,
            )?;
            check.check()?;
            // Nonblocking flock failure releases the local barrier before waiting.
            // A separate description is essential: flock is per open description.
            loop {
                let gate = mutex(&self.commit_gate, &check)?;
                let started = clock.now();
                let remaining = budget.writer_delay_limit.saturating_sub(delay);
                let barrier = Check {
                    id: check.id,
                    deadline: budget.deadline.min(started + remaining),
                    cancel,
                    clock,
                };
                let write = match disk.write.try_lock() {
                    Ok(write) => write,
                    Err(TryLockError::Poisoned(_)) => return Err(check.unhealthy()),
                    Err(TryLockError::WouldBlock) => {
                        drop(gate);
                        delay += clock.now().duration_since(started);
                        check.check()?;
                        if delay >= budget.writer_delay_limit {
                            return Err(check.expired());
                        }
                        clock.wait();
                        continue;
                    }
                };
                barrier.check()?;
                if disk.poison.is_set() {
                    return Err(check.unhealthy());
                }
                let acquired = descriptor
                    .file
                    .try_lock_exclusive()
                    .map_err(|_| check.verify())?;
                if !acquired {
                    drop(write);
                    drop(gate);
                    delay += clock.now().duration_since(started);
                    if delay >= budget.writer_delay_limit {
                        return Err(check.expired());
                    }
                    check.check()?;
                    clock.wait();
                    continue;
                }
                let flock = HeldFlock {
                    file: Arc::clone(&descriptor.file),
                    id: descriptor.id,
                };
                let valid = (|| {
                    barrier.check()?;
                    let current = disk.fs.stat_id(&disk.path).map_err(|_| check.verify())?;
                    if current != descriptor.id {
                        return Ok(false);
                    }
                    let current = publication(flock.file(), &barrier)?;
                    if disk.poison.is_set() {
                        return Err(check.unhealthy());
                    }
                    barrier.check()?;
                    Ok(current == published)
                })();
                drop(flock);
                drop(write);
                drop(gate);
                delay += clock.now().duration_since(started);
                check.check()?;
                if delay >= budget.writer_delay_limit {
                    return Err(check.expired());
                }
                if valid? {
                    return Ok(CapturedStore {
                        state,
                        publication: published,
                        retained,
                        preparation: preparation_peak,
                        writer_delay: delay,
                        source_file_identity: descriptor.id,
                        captured_at_unix_ms: std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map_err(|_| check.verify())?
                            .as_millis(),
                    });
                }
                break;
            }
        }
        Err(RecoveryError::Stale {
            operation_id: check.id,
        })
    }
}

/// Fully prepare outside the barrier. On-disk lengths bound metadata allocation
/// before decode; row expansion has its own explicit share of the same budget.
/// Graph replay receives a conservative preflight bound before any mutation.
fn prepare(
    file: &dyn ContainerIo,
    sb: &Superblock,
    budget: &CaptureBudget,
    c: &Check<'_>,
) -> Result<(DocState, u64), RecoveryError> {
    let input = add(sb.snapshot_length, sb.wal_length, c, "preparation memory")?;
    // Two payload copies (raw/decoded or WAL/replay) with 2x safety covers
    // escaped JSON scratch and Vec growth. Metadata is charged by actual
    // counts/JSON shape BEFORE allocation, not by an opaque input multiplier.
    // Column codecs account all expansion before allocation; reserve twice
    // their permitted scratch, independently of compressed input size.
    let row_scratch = budget.preparation_byte_limit / 8;
    let mut memory = PreparationMemory {
        bytes: add(
            add(
                mul(input, 4, c, "preparation memory")?,
                65536,
                c,
                "preparation memory",
            )?,
            mul(row_scratch, 2, c, "preparation memory")?,
            c,
            "preparation memory",
        )?,
        limit: budget.preparation_byte_limit,
        check: c,
    };
    memory.charge(0)?;
    let row_limits = super::columnar::Limits {
        max_rows: super::columnar::MAX_ROWS_PER_GROUP,
        max_bytes: usize::try_from(row_scratch).map_err(|_| c.quota("row memory"))?,
    };
    let mut state = if sb.snapshot_length == 0 {
        DocState::at_seq(sb.snapshot_seq)
    } else {
        let bytes = read_extent(file, sb.snapshot_offset, sb.snapshot_length, c)?;
        let snap = layout::decode_snapshot_for_recovery(
            &bytes,
            &row_limits,
            &mut || codec_progress(c),
            &mut |n| memory.codec_charge(n),
        )
        .map_err(|err| decode_error(err, c))?;
        c.check()?;
        if snap.seq != sb.snapshot_seq {
            return Err(c.verify());
        }
        DocState {
            docs: snap.docs,
            tables: snap.tables,
            vectors: snap.vectors,
            last_seq: snap.seq,
        }
    };
    let wal = read_extent(file, sb.wal_offset, sb.wal_length, c)?;
    let (frames, consumed) =
        layout::decode_wal_for_recovery(&wal, &mut || codec_progress(c), &mut |n| {
            memory.codec_charge(n)
        })
        .map_err(|err| decode_error(err, c))?;
    c.check()?;
    if consumed != sb.wal_length {
        return Err(c.verify());
    }
    // Replay may synthesize links absent from input. Bound live + inserted
    // nodes at the maximum observed M, ID length and dimension. Deletes and
    // replacement upserts only reduce this bound. No N-squared reservation.
    let mut nodes = 0u64;
    let mut id_size = 0u64;
    let mut m = 0u64;
    let mut dim = 0u64;
    for col in state.vectors.values() {
        c.check()?;
        nodes = add(nodes, col.vectors.len() as u64, c, "graph memory")?;
        m = m.max(u64::from(col.m));
        dim = dim.max(u64::from(col.dim));
        for id in col.vectors.keys() {
            c.check()?;
            id_size = id_size.max(id.capacity() as u64);
        }
    }
    let mut graph_changes = false;
    for (_, ops) in &frames {
        for op in ops {
            c.check()?;
            match op {
                layout::Op::CreateVectorCollection {
                    m: new_m,
                    dim: new_dim,
                    ..
                } => {
                    m = m.max(u64::from(*new_m));
                    dim = dim.max(u64::from(*new_dim));
                }
                layout::Op::UpsertVector { id, .. } => {
                    nodes = add(nodes, 1, c, "graph memory")?;
                    id_size = id_size.max(id.capacity() as u64);
                    graph_changes = true;
                }
                layout::Op::DeleteVector { .. } => graph_changes = true,
                _ => {}
            }
        }
    }
    if graph_changes {
        // 2M base + MAX_LEVEL*M upper links. Four copies per link cover
        // capacity growth plus transient repair/ranking lists; another 2x is
        // the common safety factor. Per-node scratch covers search queues,
        // visited sets, map slots and all layer Vec headers. Dequantization
        // keeps only a bounded number of points, never an N*dim matrix here.
        let links = mul(
            m,
            u64::from(super::vector::MAX_LEVEL) + 2,
            c,
            "graph memory",
        )?;
        let edge = mul(add(id_size, 32, c, "graph memory")?, 8, c, "graph memory")?;
        let per_node = add(
            mul(links, edge, c, "graph memory")?,
            add(
                mul(id_size, 32, c, "graph memory")?,
                8192,
                c,
                "graph memory",
            )?,
            c,
            "graph memory",
        )?;
        memory.charge(add(
            mul(nodes, per_node, c, "graph memory")?,
            mul(dim, 64, c, "graph memory")?,
            c,
            "graph memory",
        )?)?;
    }
    let mut previous = None;
    for frame in frames {
        c.check()?;
        if frame.0 == 0 || previous.is_some_and(|prev| prev >= frame.0) {
            return Err(c.verify());
        }
        previous = Some(frame.0);
        let seq = state.last_seq;
        super::state::apply_frames_for_recovery(
            &mut state,
            seq,
            std::slice::from_ref(&frame),
            &row_limits,
            &mut || codec_progress(c),
        )
        .map_err(|err| decode_error(err, c))?;
    }
    c.check()?;
    Ok((state, memory.bytes))
}

struct PreparationMemory<'a, 'b> {
    bytes: u64,
    limit: u64,
    check: &'a Check<'b>,
}
impl PreparationMemory<'_, '_> {
    fn charge(&mut self, bytes: u64) -> Result<(), RecoveryError> {
        self.check.check()?;
        self.bytes = add(self.bytes, bytes, self.check, "preparation memory")?;
        quota(self.bytes, self.limit, self.check, "preparation memory")
    }
    fn codec_charge(&mut self, bytes: u64) -> Result<(), fl_core::StoreError> {
        self.charge(bytes)
            .map_err(|_| layout::backend("recovery preparation memory exhausted"))
    }
}
// Existing row codecs return StoreError rather than a typed resource error.
// Recognize only their fixed quota prefix; never expose source data/path detail.
fn codec_progress(c: &Check<'_>) -> Result<(), fl_core::StoreError> {
    c.check()
        .map_err(|_| layout::backend("recovery codec stopped"))
}
fn decode_error(err: fl_core::StoreError, c: &Check<'_>) -> RecoveryError {
    if let Err(stopped) = c.check() {
        return stopped;
    }
    match err {
        fl_core::StoreError::Backend(message)
            if message == "recovery preparation memory exhausted" =>
        {
            c.quota("preparation memory")
        }
        fl_core::StoreError::Backend(message)
            if message
                .starts_with("corrupt ferrostore data: row group would decode to more than ") =>
        {
            c.quota("row memory")
        }
        _ => c.verify(),
    }
}
fn read_extent(
    file: &dyn ContainerIo,
    offset: u64,
    len: u64,
    c: &Check<'_>,
) -> Result<Vec<u8>, RecoveryError> {
    c.check()?;
    let end = add(offset, len, c, "extent")?;
    if end > file.len().map_err(|_| c.verify())? {
        return Err(c.verify());
    }
    let len = usize::try_from(len).map_err(|_| c.quota("extent"))?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(len)
        .map_err(|_| c.quota("extent"))?;
    bytes.resize(len, 0);
    for (index, chunk) in bytes.chunks_mut(64 * 1024).enumerate() {
        c.check()?;
        file.read_at(chunk, offset + (index as u64) * 64 * 1024)
            .map_err(|_| c.verify())?;
    }
    c.check()?;
    Ok(bytes)
}

struct Account<'a, 'b> {
    bytes: u64,
    limit: u64,
    check: &'a Check<'b>,
}
impl Account<'_, '_> {
    fn charge(&mut self, n: u64) -> Result<(), RecoveryError> {
        self.check.check()?;
        self.bytes = add(self.bytes, n, self.check, "pin memory")?;
        quota(self.bytes, self.limit, self.check, "pin memory")
    }
    fn string(&mut self, s: &String) -> Result<(), RecoveryError> {
        self.charge(add(s.capacity() as u64, 32, self.check, "pin memory")?)
    }
    fn value(&mut self, v: &Value) -> Result<(), RecoveryError> {
        self.charge(64)?;
        match v {
            Value::String(s) => self.string(s)?,
            Value::Array(a) => {
                self.charge(mul(
                    a.capacity() as u64,
                    std::mem::size_of::<Value>() as u64,
                    self.check,
                    "pin memory",
                )?)?;
                for v in a {
                    self.value(v)?;
                }
            }
            Value::Object(m) => {
                for (key, v) in m {
                    self.charge(2048)?;
                    self.string(key)?;
                    self.value(v)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
}
fn retained_bytes(state: &DocState, limit: u64, check: &Check<'_>) -> Result<u64, RecoveryError> {
    let mut a = Account {
        bytes: 0,
        limit,
        check,
    };
    a.charge(4096)?;
    // Per map entry 2048 covers an otherwise empty BTree node (spare slots,
    // edges, allocator alignment), plus Arc headers. Duplicated collection
    // charges are deliberately conservative when several documents share it.
    for (collection, id, doc) in state.docs.iter() {
        a.charge(4096)?;
        a.string(collection)?;
        a.string(id)?;
        a.value(doc)?;
    }
    for (name, table) in &state.tables {
        a.charge(2048)?;
        a.string(name)?;
        a.charge(mul(
            table.columns.capacity() as u64,
            std::mem::size_of::<super::ColumnDef>() as u64,
            check,
            "pin memory",
        )?)?;
        for column in &table.columns {
            a.string(&column.name)?;
        }
        a.charge(mul(
            table.groups.capacity() as u64,
            24,
            check,
            "pin memory",
        )?)?;
        for group in &table.groups {
            a.charge(group.capacity() as u64)?;
        }
    }
    for (name, col) in &state.vectors {
        a.charge(2048)?;
        a.string(name)?;
        if let Some(entry) = &col.entry {
            a.string(entry)?;
        }
        for (id, record) in &col.vectors {
            a.charge(2048)?;
            a.string(id)?;
            a.charge(record.bytes.capacity() as u64)?;
            if let Some(metadata) = &record.metadata {
                a.value(metadata)?;
            }
            a.charge(mul(
                record.neighbors.capacity() as u64,
                24,
                check,
                "pin memory",
            )?)?;
            for neighbors in &record.neighbors {
                a.charge(mul(neighbors.capacity() as u64, 24, check, "pin memory")?)?;
                for neighbor in neighbors {
                    a.string(neighbor)?;
                }
            }
        }
    }
    Ok(a.bytes)
}

#[path = "recovery_validation.rs"]
mod validation;
pub(crate) use validation::expectations_from_reader;

/// A fresh compatible read-only decode; never creates, migrates, refreshes a
/// live cache or retains a disk writer. File admission is done by the caller.
pub(crate) fn decode_existing(
    file: &std::fs::File,
    limits: &OperationLimits,
) -> Result<(FerroStore, ImageDescriptor), RecoveryError> {
    let check = Check {
        id: limits.operation_id,
        deadline: limits.deadline,
        cancel: &limits.cancel,
        clock: &RealClock,
    };
    check.check()?;
    quota(
        file.metadata().map_err(|_| check.verify())?.len(),
        limits.byte_limit,
        &check,
        "image bytes",
    )?;
    let published = publication(file, &check)?;
    // Recovery images have one standalone snapshot and no WAL/trailing data.
    let end = add(
        HEADER_BYTES,
        published.sb.snapshot_length,
        &check,
        "image bytes",
    )?;
    if published.sb.snapshot_offset != HEADER_BYTES
        || published.sb.snapshot_length < 12
        || published.sb.wal_length != 0
        || published.sb.wal_offset != end
        || file.metadata().map_err(|_| check.verify())?.len() != end
    {
        return Err(check.verify());
    }
    let budget = CaptureBudget::new(
        limits.deadline,
        limits.memory_byte_limit / 2,
        limits.memory_byte_limit / 2,
        Duration::from_secs(1),
    );
    let (state, _) = prepare(file, &published.sb, &budget, &check)?;
    let mut version = [0; 4];
    file.read_exact_at(&mut version, HEADER_BYTES + 8)
        .map_err(|_| check.verify())?;
    let descriptor = validation::descriptor(&state, published, 0, &check)?;
    if u32::from_le_bytes(version) != descriptor.snapshot_format {
        return Err(check.verify());
    }
    let store = FerroStore::assemble(
        Inner::Memory(Mutex::new(Arc::new(state))),
        "recovery-image".into(),
    );
    Ok((store, descriptor))
}

impl CapturedStore {
    pub fn expectations(
        &self,
        profile: &crate::recovery::ValidationProfile,
        limits: &OperationLimits,
    ) -> Result<crate::recovery::ValidationExpectations, RecoveryError> {
        let check = Check {
            id: limits.operation_id,
            deadline: limits.deadline,
            cancel: &limits.cancel,
            clock: &RealClock,
        };
        let descriptor = validation::descriptor(&self.state, self.publication, 0, &check)?;
        let store = FerroStore::assemble(
            Inner::Memory(Mutex::new(Arc::clone(&self.state))),
            "recovery-pin".into(),
        );
        let mut expected =
            validation::expectations_from_reader(&store, descriptor, profile, limits)?;
        expected.source_file_identity = Some(self.source_file_identity);
        expected.captured_at_unix_ms = Some(self.captured_at_unix_ms);
        Ok(expected)
    }
}

#[cfg(test)]
struct FakeClock {
    start: Instant,
    elapsed: Mutex<Duration>,
}
#[cfg(test)]
impl FakeClock {
    fn new() -> Self {
        Self {
            start: Instant::now(),
            elapsed: Mutex::new(Duration::ZERO),
        }
    }
}
#[cfg(test)]
impl Clock for FakeClock {
    fn now(&self) -> Instant {
        self.start + *self.elapsed.lock().unwrap()
    }
    fn wait(&self) {
        *self.elapsed.lock().unwrap() += Duration::from_millis(1);
    }
}

#[cfg(test)]
mod tests {
    use super::super::crash_sim::{IoOp, SimFs};
    use super::super::{
        ColumnDef, ColumnType, ColumnValue, FerroStore, Inner, VectorMetric, VectorStorage,
        lock_mutex,
    };
    use super::*;
    use crate::recovery::{CancelToken, CaptureBudget, OperationLimits, RecoveryError};
    use serde_json::json;
    use std::path::Path;
    use std::time::{Duration, Instant};

    #[test]
    fn capture_labels_external_outcome_unknown() {
        // Two independent descriptions model two physical openers. They share
        // the filesystem and flock, but neither cache nor per-handle Poison.
        let fs = SimFs::default();
        let path = Path::new("/sim/recovery-source.fls");
        let owner = FerroStore::open_with(&fs, path).unwrap();
        let external = FerroStore::open_with(&fs, path).unwrap();
        external
            .put_doc("unknown", "acknowledged", &json!(1))
            .unwrap();
        assert_eq!(owner.last_seq().unwrap(), 1);
        // A successful publication and a failed final sync leave identical
        // readable bytes. The capture owner has no persisted outcome bit.
        let successful_fs = SimFs::new(&fs.durable_image(), None);
        let successful_writer = FerroStore::open_with(&successful_fs, path).unwrap();
        successful_writer
            .put_doc("unknown", "unacknowledged", &json!(2))
            .unwrap();
        let mut syncs = 0;
        fs.fail_next(move |op| {
            if matches!(op, IoOp::SyncData { .. }) {
                syncs += 1;
            }
            syncs == 2
        });
        assert!(
            external
                .put_doc("unknown", "unacknowledged", &json!(2))
                .is_err()
        );
        let Inner::Disk(external_disk) = &external.inner else {
            unreachable!()
        };
        assert!(external_disk.poison.is_set());
        assert_eq!(external.last_seq().unwrap(), 1);
        assert_eq!(fs.live_image(), successful_fs.live_image());
        assert_ne!(fs.durable_image(), successful_fs.durable_image());

        // P1 must refuse the second opener instead. Until then capture
        // labels the lack of exclusion proof, even for a healthy local owner.
        let captured = owner
            .capture_recovery(&budget(), &CancelToken::new())
            .unwrap();
        assert_eq!(captured.sequence(), 2);
        assert_eq!(
            captured.external_publish_status(),
            ExternalPublishStatus::ExclusionUnprovenBeforeSingleOwnership
        );
        let tmp = tempfile::tempdir().unwrap();
        let image = tmp.path().join("image.fls");
        let mut file = std::fs::File::create(&image).unwrap();
        let descriptor = captured.write_image(&mut file, &limits()).unwrap();
        assert_eq!(
            descriptor.external_publish_status,
            captured.external_publish_status()
        );
        drop(file);
        let reopened = FerroStore::open(&image).unwrap();
        assert_eq!(reopened.last_seq().unwrap(), 2);
        assert_eq!(
            reopened.get_doc("unknown", "unacknowledged").unwrap(),
            Some(json!(2))
        );
        let durable = SimFs::new(&fs.durable_image(), None);
        assert_eq!(
            FerroStore::open_with(&durable, path)
                .unwrap()
                .last_seq()
                .unwrap(),
            1
        );
    }

    fn budget() -> CaptureBudget {
        CaptureBudget::new(
            Instant::now() + Duration::from_secs(10),
            64 << 20,
            128 << 20,
            Duration::from_secs(1),
        )
    }
    fn limits() -> OperationLimits {
        OperationLimits::new(
            Instant::now() + Duration::from_secs(10),
            64 << 20,
            128 << 20,
            CancelToken::new(),
        )
    }
    fn sim() -> (SimFs, FerroStore) {
        let fs = SimFs::default();
        let store = FerroStore::open_with(&fs, Path::new("/sim/source.fls")).unwrap();
        (fs, store)
    }
    #[test]
    fn capture_keeps_one_committed_sequence() {
        let (_fs, store) = sim();
        store
            .put_doc("unknown", "receipt", &json!({"status":"committed"}))
            .unwrap();
        store
            .create_column_table(
                "unknown_table",
                &[ColumnDef {
                    name: "value".into(),
                    ty: ColumnType::Int64,
                }],
            )
            .unwrap();
        store
            .append_column_rows("unknown_table", &[vec![ColumnValue::Int64(7)]])
            .unwrap();
        store
            .create_vector_collection(
                "unknown_vector",
                2,
                VectorStorage::F32,
                VectorMetric::L2,
                4,
                8,
                8,
            )
            .unwrap();
        store
            .upsert_vector(
                "unknown_vector",
                "v",
                &[1., 2.],
                Some(&json!({"receipt":7})),
            )
            .unwrap();
        let capture = store
            .capture_recovery(&budget(), &CancelToken::new())
            .unwrap();
        let before = super::super::encode_state_snapshot(&capture.state).unwrap();
        assert_eq!(capture.sequence(), 5);
        store
            .put_doc("unknown", "receipt", &json!("later"))
            .unwrap();
        store
            .append_column_rows("unknown_table", &[vec![ColumnValue::Int64(8)]])
            .unwrap();
        store
            .upsert_vector("unknown_vector", "v", &[9., 9.], None)
            .unwrap();
        store.delete_vector("unknown_vector", "v").unwrap();
        store.checkpoint().unwrap();
        assert_eq!(
            super::super::encode_state_snapshot(&capture.state).unwrap(),
            before
        );
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("mixed-image.fls");
        let mut file = std::fs::File::create(&path).unwrap();
        capture.write_image(&mut file, &limits()).unwrap();
        drop(file);
        let reopened = FerroStore::open(path).unwrap();
        assert_eq!(
            super::super::encode_state_snapshot(&reopened.read_state().unwrap()).unwrap(),
            before
        );
        assert!(capture.retained_bytes() > before.len() as u64);
    }
    #[test]
    fn capture_survives_external_compaction() {
        let (fs, owner) = sim();
        owner.put_doc("unknown", "id", &json!(1)).unwrap();
        let capture = owner
            .capture_recovery(&budget(), &CancelToken::new())
            .unwrap();
        let external = FerroStore::open_with(&fs, Path::new("/sim/source.fls")).unwrap();
        external.put_doc("unknown", "id", &json!(2)).unwrap();
        external.compact().unwrap();
        let refreshed = owner
            .capture_recovery(&budget(), &CancelToken::new())
            .unwrap();
        assert_eq!(capture.sequence(), 1);
        assert_eq!(capture.state.docs.get("unknown", "id"), Some(&json!(1)));
        assert_eq!(refreshed.sequence(), 2);
        assert_eq!(refreshed.state.docs.get("unknown", "id"), Some(&json!(2)));
    }
    #[test]
    fn capture_compaction_during_preparation_reopens_current_file() {
        let (fs, owner) = sim();
        owner.put_doc("c", "id", &json!(1)).unwrap();
        let external = FerroStore::open_with(&fs, Path::new("/sim/source.fls")).unwrap();
        fs.after_next_read(
            |op| matches!(op, IoOp::Read { offset, .. } if *offset >= HEADER_BYTES),
            move || {
                external.compact().unwrap();
                external.put_doc("c", "id", &json!(2)).unwrap();
            },
        );
        let capture = owner
            .capture_recovery(&budget(), &CancelToken::new())
            .unwrap();
        assert_eq!(capture.sequence(), 2);
        assert_eq!(capture.state.docs.get("c", "id"), Some(&json!(2)));
    }

    #[test]
    fn capture_stale_once_then_success_reprepares_contents() {
        let (fs, owner) = sim();
        owner.put_doc("c", "id", &json!(1)).unwrap();
        let external = FerroStore::open_with(&fs, Path::new("/sim/source.fls")).unwrap();
        let mark = fs.trace().len();
        fs.after_next_read(
            |op| matches!(op, IoOp::Read { offset, .. } if *offset >= HEADER_BYTES),
            move || external.put_doc("c", "id", &json!(2)).unwrap(),
        );
        let capture = owner
            .capture_recovery(&budget(), &CancelToken::new())
            .unwrap();
        assert_eq!(capture.sequence(), 2);
        assert_eq!(capture.state.docs.get("c", "id"), Some(&json!(2)));
        // Both attempts decoded their own WAL; the source handle's cache stayed old.
        assert!(
            fs.trace()[mark..]
                .iter()
                .filter(|op| matches!(op,
            IoOp::Read { offset, .. } if *offset >= HEADER_BYTES))
                .count()
                >= 2
        );
        let Inner::Disk(disk) = &owner.inner else {
            unreachable!()
        };
        assert_eq!(lock_mutex(&disk.cache).as_ref().unwrap().state.last_seq, 1);
    }

    #[test]
    fn capture_write_contention_releases_gate_before_wait() {
        struct ReleaseClock<'a> {
            clock: FakeClock,
            owner: &'a FerroStore,
            held: Mutex<Option<MutexGuard<'a, Descriptor>>>,
        }
        impl Clock for ReleaseClock<'_> {
            fn now(&self) -> Instant {
                self.clock.now()
            }
            fn wait(&self) {
                assert!(
                    self.owner.commit_gate.try_lock().is_ok(),
                    "capture waited while holding commit_gate"
                );
                self.held.lock().unwrap().take();
                self.clock.wait();
            }
        }
        let (_fs, owner) = sim();
        owner.put_doc("c", "id", &json!(1)).unwrap();
        let Inner::Disk(disk) = &owner.inner else {
            unreachable!()
        };
        let clock = ReleaseClock {
            clock: FakeClock::new(),
            owner: &owner,
            held: Mutex::new(Some(lock_mutex(&disk.write))),
        };
        let capture = owner
            .capture_recovery_with_clock(&budget(), &CancelToken::new(), &clock)
            .unwrap();
        assert_eq!(capture.sequence(), 1);
        assert_eq!(capture.state.docs.get("c", "id"), Some(&json!(1)));
        assert!(clock.clock.now() > clock.clock.start);
        assert!(capture.writer_delay() <= Duration::from_secs(1));
    }

    #[test]
    fn capture_rejects_poison() {
        let (fs, store) = sim();
        fs.fail_next(|op| matches!(op, IoOp::SyncData { .. }));
        assert!(store.put_doc("c", "id", &json!(1)).is_err());
        assert!(matches!(
            store.capture_recovery(&budget(), &CancelToken::new()),
            Err(RecoveryError::UnhealthySource { .. })
        ));
    }
    #[test]
    fn capture_budget_refuses_before_long_barrier() {
        let (_fs, store) = sim();
        let _held = lock_mutex(&store.commit_gate);
        let clock = FakeClock::new();
        let mut b = budget();
        b.deadline = clock.now() + Duration::from_millis(25);
        assert!(matches!(
            store.capture_recovery_with_clock(&b, &CancelToken::new(), &clock),
            Err(RecoveryError::Deadline { .. })
        ));
        assert!(clock.now().duration_since(clock.start) <= Duration::from_millis(26));
    }
    #[test]
    fn capture_cancellation_and_memory_refusal() {
        let (_fs, store) = sim();
        let cancel = CancelToken::new();
        cancel.cancel();
        assert!(matches!(
            store.capture_recovery(&budget(), &cancel),
            Err(RecoveryError::Cancelled { .. })
        ));
        store.put_doc("c", "id", &json!("x".repeat(8192))).unwrap();
        let mut b = budget();
        b.pin_byte_limit = 1;
        assert!(matches!(
            store.capture_recovery(&b, &CancelToken::new()),
            Err(RecoveryError::Quota { .. })
        ));
        assert!(matches!(
            FerroStore::in_memory().capture_recovery(&budget(), &CancelToken::new()),
            Err(RecoveryError::Unsupported { .. })
        ));
    }
    #[test]
    fn destination_failure_never_poisons_source() {
        struct Bad;
        impl RecoveryImageWriter for Bad {
            fn write_at(&mut self, _: u64, _: &[u8]) -> std::io::Result<()> {
                Err(std::io::Error::other("destination"))
            }
            fn set_len(&mut self, _: u64) -> std::io::Result<()> {
                Err(std::io::Error::other("destination"))
            }
            fn sync_all(&mut self) -> std::io::Result<()> {
                Err(std::io::Error::other("destination"))
            }
        }
        let (_fs, store) = sim();
        let captured = store
            .capture_recovery(&budget(), &CancelToken::new())
            .unwrap();
        assert!(matches!(
            captured.write_image(&mut Bad, &limits()),
            Err(RecoveryError::Destination { .. })
        ));
        store.put_doc("c", "ok", &json!(true)).unwrap();
    }
    #[test]
    fn capture_never_writes_or_syncs_source() {
        let (fs, store) = sim();
        let start = fs.trace().len();
        store
            .capture_recovery(&budget(), &CancelToken::new())
            .unwrap();
        assert!(fs.trace()[start..].iter().all(|op| !matches!(
            op,
            IoOp::Write { .. }
                | IoOp::SyncData { .. }
                | IoOp::SyncAll { .. }
                | IoOp::SetLen { .. }
                | IoOp::SyncDir { .. }
        )));
    }
    #[test]
    fn capture_flock_admission_is_deadline_aware() {
        use super::super::container_io::ContainerFs;
        let (fs, store) = sim();
        let external = fs.open_rw(Path::new("/sim/source.fls")).unwrap();
        external.lock_exclusive().unwrap();
        let clock = FakeClock::new();
        let mut b = budget();
        b.deadline = clock.now() + Duration::from_millis(20);
        assert!(matches!(
            store.capture_recovery_with_clock(&b, &CancelToken::new(), &clock),
            Err(RecoveryError::Deadline { .. })
        ));
        assert!(store.commit_gate.try_lock().is_ok());
        external.unlock();
        store.put_doc("c", "after", &json!(true)).unwrap();
    }
    #[test]
    fn capture_refuses_slow_barrier_using_simulated_elapsed_time() {
        let (fs, store) = sim();
        let clock = Arc::new(FakeClock::new());
        let advanced = Arc::clone(&clock);
        // Two slot reads prepare; the third is under the capture flock.
        let mut reads = 0;
        fs.after_next_read(
            move |op| {
                if matches!(op, IoOp::Read { len, .. } if *len == SLOT_LEN as u64) {
                    reads += 1;
                }
                reads == 3
            },
            move || {
                *advanced.elapsed.lock().unwrap() += Duration::from_secs(2);
            },
        );
        assert!(matches!(
            store.capture_recovery_with_clock(&budget(), &CancelToken::new(), &*clock),
            Err(RecoveryError::Deadline { .. })
        ));
        store.put_doc("c", "after", &json!(true)).unwrap();
    }
    #[test]
    fn capture_cold_preparation_refuses_before_extent_allocation() {
        let (fs, owner) = sim();
        let external = FerroStore::open_with(&fs, Path::new("/sim/source.fls")).unwrap();
        external
            .put_doc("c", "large", &json!("x".repeat(65536)))
            .unwrap();
        let start = fs.trace().len();
        let mut b = budget();
        b.preparation_byte_limit = 1024;
        assert!(matches!(
            owner.capture_recovery(&b, &CancelToken::new()),
            Err(RecoveryError::Quota { .. })
        ));
        assert!(fs.trace()[start..].iter().all(|op| !matches!(op,
            IoOp::Read { offset, .. } if *offset >= HEADER_BYTES)));
    }
    #[test]
    fn capture_sync_publish_keeps_the_old_pin() {
        use super::super::{SyncBatch, SyncOp};
        let (_fs, store) = sim();
        let captured = store
            .capture_recovery(&budget(), &CancelToken::new())
            .unwrap();
        store
            .apply_inbound(vec![
                SyncBatch {
                    seq: 1,
                    ops: vec![SyncOp::PutDoc {
                        collection: "unknown".into(),
                        id: "a".into(),
                        doc: json!(1),
                    }],
                },
                SyncBatch {
                    seq: 2,
                    ops: vec![SyncOp::PutDoc {
                        collection: "unknown".into(),
                        id: "b".into(),
                        doc: json!(2),
                    }],
                },
            ])
            .unwrap();
        assert_eq!(captured.sequence(), 0);
        assert!(captured.state.docs.get("unknown", "a").is_none());
        assert_eq!(
            store
                .capture_recovery(&budget(), &CancelToken::new())
                .unwrap()
                .sequence(),
            2
        );
    }
    #[test]
    fn capture_group_commits_never_split_multi_op_transactions() {
        let (_fs, store) = sim();
        std::thread::scope(|scope| {
            let start = Arc::new(std::sync::Barrier::new(9));
            for n in 0..8 {
                let start = Arc::clone(&start);
                let store = &store;
                scope.spawn(move || {
                    start.wait();
                    store
                        .with_write(|txn| {
                            txn.put("left", n.to_string(), json!(n));
                            txn.put("right", n.to_string(), json!(n));
                            Ok(())
                        })
                        .unwrap();
                });
            }
            start.wait();
            for _ in 0..8 {
                if let Ok(captured) = store.capture_recovery(&budget(), &CancelToken::new()) {
                    let left: Vec<_> = captured.state.docs.in_collection("left").collect();
                    let right: Vec<_> = captured.state.docs.in_collection("right").collect();
                    assert_eq!(left, right);
                    assert_eq!(left.len() as u64, captured.sequence());
                }
            }
        });
        assert_eq!(
            store
                .capture_recovery(&budget(), &CancelToken::new())
                .unwrap()
                .sequence(),
            8
        );
    }
    #[test]
    fn image_preserves_container_format_and_missing_schema() {
        use super::super::crash_sim::Image;
        let docs = super::super::Docs::default();
        let blob = layout::encode_snapshot(42, &docs).unwrap();
        let sb = Superblock {
            epoch: 3,
            snapshot_offset: HEADER_BYTES,
            snapshot_length: blob.len() as u64,
            snapshot_seq: 42,
            wal_offset: HEADER_BYTES + blob.len() as u64,
            wal_length: 0,
        };
        let mut bytes = vec![0u8; HEADER_BYTES as usize];
        bytes[..SLOT_LEN].copy_from_slice(&layout::encode_superblock_as(&sb, 1, 0, 0));
        bytes.extend_from_slice(&blob);
        let fs = SimFs::new(&Image::single(Path::new("/sim/source.fls"), bytes), None);
        let store = FerroStore::open_with(&fs, Path::new("/sim/source.fls")).unwrap();
        let captured = store
            .capture_recovery(&budget(), &CancelToken::new())
            .unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("image.fls");
        let mut file = std::fs::File::create(&path).unwrap();
        let d = captured.write_image(&mut file, &limits()).unwrap();
        assert_eq!(d.container_format, 1);
        assert_eq!(d.schema, None);
        assert_eq!(d.lineage, None);
        drop(file);
        let reopened = FerroStore::open(path).unwrap();
        assert_eq!(reopened.last_seq().unwrap(), 42);
        assert_eq!(reopened.format_version().unwrap(), None);
    }
    #[test]
    fn image_enforces_quotas_and_cancellation_before_io() {
        struct Untouched;
        impl RecoveryImageWriter for Untouched {
            fn write_at(&mut self, _: u64, _: &[u8]) -> io::Result<()> {
                panic!("write")
            }
            fn set_len(&mut self, _: u64) -> io::Result<()> {
                panic!("truncate")
            }
            fn sync_all(&mut self) -> io::Result<()> {
                panic!("sync")
            }
        }
        let (_fs, store) = sim();
        let captured = store
            .capture_recovery(&budget(), &CancelToken::new())
            .unwrap();
        let mut l = limits();
        l.memory_byte_limit = 1;
        assert!(matches!(
            captured.write_image(&mut Untouched, &l),
            Err(RecoveryError::Quota { .. })
        ));
        let mut l = limits();
        l.byte_limit = HEADER_BYTES - 1;
        assert!(matches!(
            captured.write_image(&mut Untouched, &l),
            Err(RecoveryError::Quota { .. })
        ));
        let l = limits();
        l.cancel.cancel();
        assert!(matches!(
            captured.write_image(&mut Untouched, &l),
            Err(RecoveryError::Cancelled { .. })
        ));
    }

    #[test]
    fn capture_compressed_row_budget_is_quota_not_unhealthy_source() {
        let (fs, owner) = sim();
        let external = FerroStore::open_with(&fs, Path::new("/sim/source.fls")).unwrap();
        external
            .create_column_table(
                "compressed",
                &[ColumnDef {
                    name: "b".into(),
                    ty: ColumnType::Utf8,
                }],
            )
            .unwrap();
        external
            .append_column_rows(
                "compressed",
                &vec![vec![ColumnValue::Utf8("x".repeat(2048))]; 10_000],
            )
            .unwrap();
        external.checkpoint().unwrap();
        let mut b = budget();
        b.preparation_byte_limit = 16 << 20;
        assert!(matches!(
            owner.capture_recovery(&b, &CancelToken::new()),
            Err(RecoveryError::Quota { .. })
        ));
        external
            .put_doc("c", "still_healthy", &json!(true))
            .unwrap();
    }

    #[test]
    fn capture_dense_json_shape_is_charged_before_decode() {
        let (_fs, owner) = sim();
        owner
            .put_doc("dense", "array", &json!(vec![false; 50_000]))
            .unwrap();
        let mut b = budget();
        b.preparation_byte_limit = 16 << 20;
        assert!(matches!(
            owner.capture_recovery(&b, &CancelToken::new()),
            Err(RecoveryError::Quota {
                resource: "preparation memory",
                ..
            })
        ));
        // The ordinary read path remains unaffected by recovery admission.
        assert_eq!(
            owner
                .get_doc("dense", "array")
                .unwrap()
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            50_000
        );
    }

    #[test]
    fn capture_stale_preparation_retries_three_after_initial_attempt() {
        fn arm(
            fs: SimFs,
            writer: Arc<FerroStore>,
            count: Arc<std::sync::atomic::AtomicUsize>,
            skip: usize,
        ) {
            let callback_fs = fs.clone();
            let mut reads = 0;
            fs.after_next_read(
                move |op| {
                    if matches!(op, IoOp::Read { len, .. } if *len == SLOT_LEN as u64) {
                        reads += 1;
                    }
                    reads == skip
                },
                move || {
                    let n = count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    writer.put_doc("c", "external", &json!(n)).unwrap();
                    arm(callback_fs, writer, count, 4);
                },
            );
        }
        let (fs, owner) = sim();
        let external = Arc::new(FerroStore::open_with(&fs, Path::new("/sim/source.fls")).unwrap());
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        arm(fs, external, Arc::clone(&count), 2);
        let Err(RecoveryError::Stale { operation_id }) =
            owner.capture_recovery(&budget(), &CancelToken::new())
        else {
            panic!("stale retry exhaustion must be distinct");
        };
        assert!(!operation_id.is_nil());
        assert_eq!(count.load(std::sync::atomic::Ordering::Relaxed), 4);
    }
    #[test]
    fn image_io_releases_source_barrier_and_observes_cancellation() {
        struct Pause {
            entered: std::sync::mpsc::Sender<()>,
            resume: std::sync::mpsc::Receiver<()>,
        }
        impl RecoveryImageWriter for Pause {
            fn write_at(&mut self, _: u64, _: &[u8]) -> io::Result<()> {
                Ok(())
            }
            fn set_len(&mut self, _: u64) -> io::Result<()> {
                self.entered.send(()).unwrap();
                self.resume.recv_timeout(Duration::from_secs(10)).unwrap();
                Ok(())
            }
            fn sync_all(&mut self) -> io::Result<()> {
                panic!("cancelled write must not sync")
            }
        }
        let (_fs, store) = sim();
        let captured = store
            .capture_recovery(&budget(), &CancelToken::new())
            .unwrap();
        let l = limits();
        let cancel = l.cancel.clone();
        let (entered, observed) = std::sync::mpsc::channel();
        let (resume, next) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            let job = scope.spawn(move || {
                captured.write_image(
                    &mut Pause {
                        entered,
                        resume: next,
                    },
                    &l,
                )
            });
            observed.recv_timeout(Duration::from_secs(10)).unwrap();
            store
                .put_doc("c", "while_image_paused", &json!(true))
                .unwrap();
            cancel.cancel();
            resume.send(()).unwrap();
            assert!(matches!(
                job.join().unwrap(),
                Err(RecoveryError::Cancelled { .. })
            ));
        });
    }
    #[test]
    fn pin_accounting_includes_spare_heap_capacity() {
        let mut state = DocState::default();
        state.docs.insert(
            "unknown".into(),
            "empty_but_reserved".into(),
            Arc::new(Value::Array(Vec::with_capacity(100_000))),
        );
        let clock = RealClock;
        let cancel = CancelToken::new();
        let check = Check {
            id: Uuid::now_v7(),
            deadline: Instant::now() + Duration::from_secs(10),
            cancel: &cancel,
            clock: &clock,
        };
        assert!(super::super::encode_state_snapshot(&state).unwrap().len() < 200);
        assert!(matches!(
            retained_bytes(&state, 1 << 20, &check),
            Err(RecoveryError::Quota { .. })
        ));
        assert!(
            retained_bytes(&state, 16 << 20, &check).unwrap()
                >= 100_000 * std::mem::size_of::<Value>() as u64
        );
    }
    #[test]
    fn capture_excludes_ephemeral_mq_without_flushing() {
        use super::super::{MqDurability, MqResourceProbe, MqResourceSnapshot, MqTierPolicy};
        struct Ample;
        impl MqResourceProbe for Ample {
            fn probe(&self, _: &Path) -> MqResourceSnapshot {
                MqResourceSnapshot {
                    available_ram_bytes: Some(u64::MAX),
                    free_disk_bytes: Some(u64::MAX),
                }
            }
        }
        let (fs, store) = sim();
        store
            .configure_mq(MqTierPolicy::default(), false, Arc::new(Ample))
            .unwrap();
        store
            .publish_mq_with(
                "temporary",
                "key",
                &json!("ephemeral"),
                MqDurability::Ephemeral,
            )
            .unwrap();
        let before = fs.live_image();
        let captured = store
            .capture_recovery(&budget(), &CancelToken::new())
            .unwrap();
        assert_eq!(captured.sequence(), 0);
        assert!(captured.state.docs.get("mq:temporary", "1").is_none());
        assert_eq!(fs.live_image(), before);
        assert_eq!(
            store.poll_mq_group("temporary", "group", 10).unwrap().len(),
            1
        );
    }
    #[test]
    fn snapshot_codecs_stop_between_records() {
        let mut state = DocState::default();
        for n in 0..100 {
            state
                .docs
                .insert("c".into(), n.to_string(), Arc::new(json!(n)));
        }
        let blob = super::super::encode_state_snapshot(&state).unwrap();
        let cancel = CancelToken::new();
        let clock = RealClock;
        let check = Check {
            id: Uuid::now_v7(),
            deadline: Instant::now() + Duration::from_secs(10),
            cancel: &cancel,
            clock: &clock,
        };
        let mut seen = 0;
        let encoded = layout::encode_snapshot_for_recovery(&state, &mut || {
            seen += 1;
            if seen == 10 {
                cancel.cancel();
            }
            codec_progress(&check)
        });
        assert!(matches!(
            encoded.map_err(|e| decode_error(e, &check)),
            Err(RecoveryError::Cancelled { .. })
        ));
        assert_eq!(seen, 10);
        let cancel = CancelToken::new();
        let check = Check {
            cancel: &cancel,
            ..check
        };
        let mut seen = 0;
        let decoded = layout::decode_snapshot_for_recovery(
            &blob,
            &super::super::columnar::Limits::DEFAULT,
            &mut || {
                seen += 1;
                if seen == 10 {
                    cancel.cancel();
                }
                codec_progress(&check)
            },
            &mut |_| Ok(()),
        );
        assert!(matches!(
            decoded.map_err(|e| decode_error(e, &check)),
            Err(RecoveryError::Cancelled { .. })
        ));
        assert_eq!(seen, 10);
    }
    #[test]
    fn destination_short_write_and_sync_failure_leave_source_healthy() {
        struct FailAt {
            step: u8,
        }
        impl RecoveryImageWriter for FailAt {
            fn set_len(&mut self, _: u64) -> io::Result<()> {
                Ok(())
            }
            fn write_at(&mut self, _: u64, _: &[u8]) -> io::Result<()> {
                if self.step == 0 {
                    Err(io::Error::new(io::ErrorKind::WriteZero, "short write"))
                } else {
                    Ok(())
                }
            }
            fn sync_all(&mut self) -> io::Result<()> {
                Err(io::Error::other("sync failed"))
            }
        }
        let (_fs, store) = sim();
        for step in [0, 1] {
            let capture = store
                .capture_recovery(&budget(), &CancelToken::new())
                .unwrap();
            assert!(matches!(
                capture.write_image(&mut FailAt { step }, &limits()),
                Err(RecoveryError::Destination { .. })
            ));
            store
                .put_doc("c", "after_destination_failure", &json!(step))
                .unwrap();
        }
    }
}
