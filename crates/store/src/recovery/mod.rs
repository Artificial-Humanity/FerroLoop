//! Recovery inventory. Backup, restore and ownership cutovers are separate work.
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
    #[error("inventory metadata unavailable: {0}")]
    Metadata(String),
}
