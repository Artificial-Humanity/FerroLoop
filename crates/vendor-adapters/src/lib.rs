//! Concrete [`fl_core::VendorHostAdapter`] implementations composed at `fl-cli`.
//!
//! `fl-hub`, `fl-wire`, and `fl-host` depend only on the pure traits and domain types in
//! `fl-core::vendor_adapter` and never on this crate.

pub mod codex;

pub use codex::CodexHostAdapter;

use std::sync::Arc;

use fl_core::VendorHostAdapterRegistry;

/// Build a [`VendorHostAdapterRegistry`] pre-populated with all concrete vendor host adapters
/// implemented in this crate.
pub fn default_vendor_host_adapter_registry() -> VendorHostAdapterRegistry {
    let registry = VendorHostAdapterRegistry::new();
    registry.register(Arc::new(CodexHostAdapter::new()));
    registry
}
