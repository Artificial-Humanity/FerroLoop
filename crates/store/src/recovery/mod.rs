//! Recovery inventory and explicitly bounded physical capture primitives.
pub mod admission;
pub mod backup;
pub mod catalog;
pub mod fs;
pub mod inventory;
mod inventory_metadata;
mod inventory_types;
pub mod purge;
pub mod readiness;
pub mod restore;
mod restore_metadata;
pub use inventory::{
    BindingEdge, BindingInput, ConsumerEdge, InventoryInputs, StoreIdentity, StoreMap,
    collect_inventory,
};

#[derive(Debug, thiserror::Error)]
pub enum RecoveryError {
    #[error(
        "restore admission fenced by {guard_path:?} (operation {operation_id:?}); inspect with `ferrostore recovery --root <ROOT> --lease <LEASE> --request <CONTINUATION.json> status`; administrative exclusion and reconciliation required"
    )]
    RestoreFenced {
        guard_path: std::path::PathBuf,
        operation_id: Option<uuid::Uuid>,
    },
    #[error("could not resolve physical store path {path:?}: {source}")]
    PhysicalPathResolution {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(
        "recovery {operation_id}: admission failed; store is {fence_state}; retained metadata requires operator review"
    )]
    RestoreAdmissionArtifactsRetained {
        operation_id: uuid::Uuid,
        fence_state: RestoreAdmissionFenceState,
        staging: std::path::PathBuf,
        staging_identity: Option<(u64, u64)>,
        operation_lock_identity: Option<(u64, u64)>,
        #[source]
        source: Box<RecoveryError>,
    },
    #[error("recovery {operation_id}: image exceeds admitted ceiling")]
    ImageExceedsCeiling { operation_id: uuid::Uuid },
    #[error("recovery {operation_id}: another backup holds the physical store lock")]
    ConcurrentBackup { operation_id: uuid::Uuid },
    #[error("recovery {operation_id}: bundle quarantined")]
    Quarantined { operation_id: uuid::Uuid },
    #[error("inventory input refused: {0}")]
    Refused(String),
    #[error("recovery {operation_id}: input refused ({reason})")]
    RefusedInput {
        operation_id: uuid::Uuid,
        reason: &'static str,
    },
    #[error("recovery {operation_id}: source health is uncertain")]
    UnhealthySource { operation_id: uuid::Uuid },
    #[error("recovery {operation_id}: {reason}")]
    Unsupported {
        operation_id: uuid::Uuid,
        reason: &'static str,
    },
    #[error("recovery {operation_id}: capture remained stale after three retries")]
    Stale { operation_id: uuid::Uuid },
    #[error("recovery {operation_id}: quota exceeded ({resource})")]
    Quota {
        operation_id: uuid::Uuid,
        resource: &'static str,
    },
    #[error(
        "recovery {operation_id}: {resource} offers {offered_bytes} encoded bytes; limit {limit_bytes} encoded bytes (JSON quotes and escaping included); journal unchanged"
    )]
    RestoreTerminalInputQuota {
        operation_id: uuid::Uuid,
        resource: &'static str,
        offered_bytes: u64,
        limit_bytes: u64,
    },
    #[error(
        "recovery {operation_id}: journal {journal:?} frame at offset {offset} declares {declared_bytes} bytes above cap {cap}; no repair or unfence attempted"
    )]
    RestoreJournalFrameQuota {
        operation_id: uuid::Uuid,
        journal: std::path::PathBuf,
        offset: u64,
        declared_bytes: u64,
        cap: u64,
    },
    #[error(
        "recovery {operation_id}: journal {journal:?} frame {frame_index} at offset {offset} declares {declared_bytes} bytes above frame count cap {cap}; no repair or unfence attempted"
    )]
    RestoreJournalFrameCountQuota {
        operation_id: uuid::Uuid,
        journal: std::path::PathBuf,
        offset: u64,
        frame_index: u64,
        cap: u64,
        declared_bytes: u64,
    },
    #[error("recovery {operation_id}: deadline exceeded")]
    Deadline { operation_id: uuid::Uuid },
    #[error("recovery {operation_id}: cancelled")]
    Cancelled { operation_id: uuid::Uuid },
    #[error("recovery {operation_id}: image or captured expectations verification failed")]
    Verification { operation_id: uuid::Uuid },
    #[error("recovery {operation_id}: metadata verification failed at {entry:?}")]
    MetadataVerification {
        operation_id: uuid::Uuid,
        /// Lexical entry path relative to the approved recovery root.
        entry: std::path::PathBuf,
        #[source]
        source: Box<RecoveryError>,
    },
    #[error("recovery {operation_id}: transient resource failure ({errno})")]
    Transient {
        operation_id: uuid::Uuid,
        errno: i32,
    },
    #[error("recovery {operation_id}: destination I/O failed")]
    Destination { operation_id: uuid::Uuid },
    #[error("recovery {operation_id}: publication outcome unknown")]
    OutcomeUnknown { operation_id: uuid::Uuid },
    #[error("inventory metadata unavailable: {0}")]
    Metadata(String),
}

/// Observation of the admission name after an interrupted admission. No guard
/// contents are read, repaired or removed to produce this diagnostic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RestoreAdmissionFenceState {
    Fenced,
    Unfenced,
    Uncertain,
}
impl std::fmt::Display for RestoreAdmissionFenceState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Fenced => "fenced",
            Self::Unfenced => "unfenced",
            Self::Uncertain => "of uncertain fence state",
        })
    }
}
impl RecoveryError {
    /// Call only after private O_NOFOLLOW descriptor checks. Construct the
    /// lexical entry path lazily: resource/authority refusals keep their type
    /// and do not allocate diagnostic paths, including on memory exhaustion.
    pub(crate) fn at_metadata_entry(self, entry: impl FnOnce() -> std::path::PathBuf) -> Self {
        match self {
            Self::Verification { operation_id } => Self::MetadataVerification {
                operation_id,
                entry: entry(),
                source: Box::new(self),
            },
            other => other,
        }
    }
}

/// A shared cooperative cancellation flag. Cancellation never writes the source.
#[derive(Clone, Debug, Default)]
pub struct CancelToken(
    std::sync::Arc<std::sync::atomic::AtomicBool>,
    Vec<CancelToken>,
);
impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn cancel(&self) {
        self.0.store(true, std::sync::atomic::Ordering::Release);
    }
    pub(crate) fn combined(a: &Self, b: &Self) -> Self {
        Self(Default::default(), vec![a.clone(), b.clone()])
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(std::sync::atomic::Ordering::Acquire) || self.1.iter().any(Self::is_cancelled)
    }
}

/// Explicit capture limits; no unbounded default. Preparation includes decoder
/// working memory and the pin. The writer delay limit may not exceed one second.
#[derive(Clone, Debug)]
pub struct CaptureBudget {
    pub deadline: std::time::Instant,
    pub pin_byte_limit: u64,
    pub preparation_byte_limit: u64,
    pub writer_delay_limit: std::time::Duration,
}
impl CaptureBudget {
    pub fn new(
        deadline: std::time::Instant,
        pin_byte_limit: u64,
        preparation_byte_limit: u64,
        writer_delay_limit: std::time::Duration,
    ) -> Self {
        Self {
            deadline,
            pin_byte_limit,
            preparation_byte_limit,
            writer_delay_limit,
        }
    }
}

/// Explicit destination and working-memory quotas for one image operation.
#[derive(Clone, Debug)]
pub struct OperationLimits {
    pub deadline: std::time::Instant,
    pub byte_limit: u64,
    pub memory_byte_limit: u64,
    pub cancel: CancelToken,
    pub operation_id: uuid::Uuid,
    /// Cumulative synchronous-query input work: JSON/key/vector/graph bytes plus
    /// one unit per row. Repeated scans are charged again; lexical query bytes
    /// are charged per input row. This is an admission bound, not CPU time.
    /// Defaults to 64 * byte_limit, conservatively capped at u64::MAX on
    /// multiplication overflow; consumption uses checked addition and never
    /// wraps. The default allows repeated automatic passes, not unlimited work.
    /// Consumption lasts for this OperationLimits instance and all its clones,
    /// including both verification opens; starting another call does not reset
    /// it. Callers may override the ceiling before use. It never relaxes the
    /// independent memory, image-byte or deadline limits. An admitted synchronous
    /// API cannot be hard-preempted; cancellation is checked before/after it and
    /// throughout input admission.
    pub query_work_limit: u64,
    // Clones and both verification opens share consumption for this operation.
    query_work_used: std::sync::Arc<std::sync::atomic::AtomicU64>,
    pub(crate) publication_io: Option<std::sync::Arc<fs::PublicationIo>>,
    memory_admission: std::sync::Arc<std::sync::atomic::AtomicU64>,
    retained_memory_bytes: u64,
}
impl OperationLimits {
    pub fn new(
        deadline: std::time::Instant,
        byte_limit: u64,
        memory_byte_limit: u64,
        cancel: CancelToken,
    ) -> Self {
        Self {
            deadline,
            byte_limit,
            memory_byte_limit,
            cancel,
            operation_id: uuid::Uuid::now_v7(),
            query_work_limit: byte_limit.saturating_mul(64),
            query_work_used: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            publication_io: None,
            memory_admission: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            retained_memory_bytes: 0,
        }
    }
}

mod capacity;
pub mod control;
pub mod manifest;
pub mod policy;
pub mod profiles;
pub mod scheduler;
pub(crate) mod verify;
pub use manifest::{BundleManifestV1, ValidationExpectations, VerificationReport};
pub use profiles::{ProfileKind, ValidationProfile};
pub use verify::{
    copy_and_verify, open_recovery_image_existing, reference_digest, verify_existing_copy,
};

impl OperationLimits {
    /// Largest admitted simultaneous working-memory reservation, including the
    /// retained reader/pin and model safety factors. Not measured RSS/heap.
    pub fn admitted_memory_bytes(&self) -> u64 {
        self.memory_admission
            .load(std::sync::atomic::Ordering::Acquire)
    }
    /// A scoped clone charges nested scratch alongside already retained values.
    /// The caller admits this reservation before allocating those values.
    pub(crate) fn with_retained_memory(&self, bytes: u64) -> Result<Self, RecoveryError> {
        let mut scoped = self.clone();
        scoped.retained_memory_bytes = self
            .retained_memory_bytes
            .checked_add(bytes)
            .ok_or_else(|| self.verification())?;
        scoped.admit_memory(0)?;
        Ok(scoped)
    }
    pub(crate) fn admit_memory(&self, bytes: u64) -> Result<(), RecoveryError> {
        let bytes = bytes
            .checked_add(self.retained_memory_bytes)
            .ok_or_else(|| self.verification())?;
        self.quota(bytes, self.memory_byte_limit, "working memory")?;
        self.memory_admission
            .fetch_max(bytes, std::sync::atomic::Ordering::AcqRel);
        Ok(())
    }
    pub(crate) fn charge_query_work(&self, units: u64) -> Result<(), RecoveryError> {
        self.check()?;
        use std::sync::atomic::Ordering::{AcqRel, Acquire};
        let mut used = self.query_work_used.load(Acquire);
        loop {
            self.check()?;
            let next = used
                .checked_add(units)
                .filter(|next| *next <= self.query_work_limit)
                .ok_or(RecoveryError::Quota {
                    operation_id: self.operation_id,
                    resource: "query work",
                })?;
            match self
                .query_work_used
                .compare_exchange_weak(used, next, AcqRel, Acquire)
            {
                Ok(_) => break,
                Err(current) => used = current,
            }
        }
        Ok(())
    }
    pub(crate) fn check(&self) -> Result<(), RecoveryError> {
        if self.cancel.is_cancelled() {
            return Err(RecoveryError::Cancelled {
                operation_id: self.operation_id,
            });
        }
        if std::time::Instant::now() >= self.deadline {
            return Err(RecoveryError::Deadline {
                operation_id: self.operation_id,
            });
        }
        Ok(())
    }
    pub(crate) fn quota(
        &self,
        bytes: u64,
        limit: u64,
        resource: &'static str,
    ) -> Result<(), RecoveryError> {
        self.check()?;
        if bytes > limit {
            Err(RecoveryError::Quota {
                operation_id: self.operation_id,
                resource,
            })
        } else {
            Ok(())
        }
    }
    pub(crate) fn verification(&self) -> RecoveryError {
        RecoveryError::Verification {
            operation_id: self.operation_id,
        }
    }
}

#[cfg(test)]
pub(crate) mod test_support;
