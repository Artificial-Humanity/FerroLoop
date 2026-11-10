//! Recovery inventory and explicitly bounded physical capture primitives.
pub mod backup;
pub mod catalog;
pub mod fs;
pub mod inventory;
mod inventory_metadata;
mod inventory_types;
pub use inventory::{
    BindingEdge, BindingInput, ConsumerEdge, InventoryInputs, StoreIdentity, StoreMap,
    collect_inventory,
};

#[derive(Debug, thiserror::Error)]
pub enum RecoveryError {
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
    #[error("recovery {operation_id}: deadline exceeded")]
    Deadline { operation_id: uuid::Uuid },
    #[error("recovery {operation_id}: cancelled")]
    Cancelled { operation_id: uuid::Uuid },
    #[error("recovery {operation_id}: image or captured expectations verification failed")]
    Verification { operation_id: uuid::Uuid },
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
    pub(crate) fn admit_memory(&self, bytes: u64) -> Result<(), RecoveryError> {
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
