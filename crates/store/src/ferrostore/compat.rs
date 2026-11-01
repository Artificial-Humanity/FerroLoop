//! Read-only questions callers ask a store beyond the thirteen traits.
//! These sit beside the engine so `AnyStore` can ask a FerroStore what it
//! already asks a RedbStore.

use super::FerroStore;
use super::doc::{self, IMPORTS, LEDGER_ROOTS};
use fl_core::ids::{Kind, ProjectId};
use fl_core::iri::Iri;
use fl_core::store::StoreError;

impl FerroStore {
    /// Whether this store holds `id`. An I/O failure is an error, never `false`.
    pub fn owns(&self, id: &Iri) -> Result<bool, StoreError> {
        self.with_read(|txn| doc::owns(txn, &self.label, id))
    }

    /// The hash of the manifest `project` was imported from, or `None` if this
    /// store authors it. A project this store never held is `NotOwned`.
    pub fn imported_hash(&self, project: &ProjectId) -> Result<Option<String>, StoreError> {
        self.with_read(|txn| {
            doc::check_kind(txn, &self.label, project.iri(), Kind::Project)?;
            doc::get_json(txn, IMPORTS, project.iri().as_str())
        })
    }

    /// Whether any project in this store has a routing map.
    pub fn holds_routing(&self) -> Result<bool, StoreError> {
        self.with_read(|txn| Ok(doc::holds_routing(txn)))
    }

    /// Whether this store records any ledger root.
    pub fn holds_a_ledger_root(&self) -> Result<bool, StoreError> {
        self.with_read(|txn| Ok(!doc::scan_json::<String>(txn, LEDGER_ROOTS)?.is_empty()))
    }
}
