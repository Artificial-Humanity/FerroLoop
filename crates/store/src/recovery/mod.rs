//! Recovery inventory and explicitly bounded physical capture primitives.
pub mod inventory;
mod inventory_metadata;
mod inventory_types;
pub use inventory::{
    BindingEdge, BindingInput, ConsumerEdge, InventoryInputs, StoreIdentity, StoreMap,
    collect_inventory,
};

#[derive(Debug, thiserror::Error)]
pub enum RecoveryError {
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
    #[error("recovery {operation_id}: destination I/O failed")]
    Destination { operation_id: uuid::Uuid },
    #[error("recovery {operation_id}: publication outcome unknown")]
    OutcomeUnknown { operation_id: uuid::Uuid },
    #[error("inventory metadata unavailable: {0}")]
    Metadata(String),
}

/// A shared cooperative cancellation flag. Cancellation never writes the source.
#[derive(Clone, Debug, Default)]
pub struct CancelToken(std::sync::Arc<std::sync::atomic::AtomicBool>);
impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn cancel(&self) {
        self.0.store(true, std::sync::atomic::Ordering::Release);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(std::sync::atomic::Ordering::Acquire)
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
        }
    }
}

pub mod manifest;
pub mod profiles;
pub(crate) mod verify;
pub use manifest::{BundleManifestV1, ValidationExpectations, VerificationReport};
pub use profiles::{ProfileKind, ValidationProfile};
pub use verify::{
    copy_and_verify, open_recovery_image_existing, reference_digest, verify_existing_copy,
};

impl OperationLimits {
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
