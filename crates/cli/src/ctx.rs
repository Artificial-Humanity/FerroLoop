//! What a command works with (GitHub tracker spec §1.3): the local store
//! for the catalog and the ledger, and whichever tracker the project is
//! bound to.

use fl_core::store::{Handles, Roles, Tracker};
use fl_store::RedbStore;

pub struct Ctx<'a> {
    pub store: &'a RedbStore,
    /// The local store, or the GitHub tracker behind `CatalogChecked`.
    pub tracker: &'a dyn Tracker,
    /// The local store's handles, or `KindRouted` over the store and GitHub.
    pub handles: &'a dyn Handles,
    /// Where records and findings live, for messages.
    pub tracker_label: String,
}

impl Ctx<'_> {
    pub fn roles(&self) -> Roles<'_> {
        Roles {
            catalog: self.store,
            tracker: self.tracker,
            ledger: self.store,
        }
    }
}
