//! Finite, shared slice admission. A reservation precedes every owned allocation.
use crate::reliable::ChargedBytes;
use fl_host_proto::ExtensionRefusal;
use std::sync::{Arc, Mutex};

pub(crate) const EPOCH_DATA: usize = 8 * 1024 * 1024;
pub(crate) const EPOCH_LIFECYCLE: usize = 16 * 1024;
pub(crate) const GLOBAL_DATA: usize = 512 * 1024 * 1024;
pub(crate) const GLOBAL_LIFECYCLE: usize = 1024 * 1024;
const EPOCH_ADDITIONAL: usize = 192 * 1024 * 1024;
const EPOCH_PROJECTION: usize = 128 * 1024 * 1024;

// Reservations count a conservative storage bound; they do not eagerly allocate
// it. Keep the old non-projection headroom while admitting every default session.
pub(crate) fn projection_limit() -> usize {
    fl_host_proto::DEFAULT_MAX_SESSIONS
        .checked_mul(crate::projection::default_storage_bound())
        .expect("default projection allowance fits usize")
}
pub(crate) fn additional_limit() -> usize {
    projection_limit()
        .checked_add(EPOCH_ADDITIONAL - EPOCH_PROJECTION)
        .expect("additional allowance fits usize")
}
pub(crate) const METADATA: usize = 8 * 1024 * 1024;

#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub(crate) enum ChargeKind {
    ReliableData,
    Lifecycle,
    Projection,
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "Approved PTY stage consumes this category")
    )]
    Ring,
    Snapshot,
    Viewer,
    Metadata,
}
#[derive(Default)]
struct Usage {
    categories: [usize; 7],
    additional: usize,
}
pub(crate) struct SliceBudget {
    usage: Mutex<Usage>,
    parent: Option<Arc<SliceBudget>>,
    _metadata: Option<Reservation>,
}
pub(crate) struct Reservation {
    budget: Arc<SliceBudget>,
    kind: ChargeKind,
    bytes: usize,
}
impl SliceBudget {
    pub(crate) fn new() -> Arc<Self> {
        // The fixed bootstrap object admits its own known metadata before its
        // first allocation. Every subsequent budget is charged to this root.
        let bytes = std::mem::size_of::<Self>() + 2 * std::mem::size_of::<usize>();
        let mut usage = Usage::default();
        usage.categories[ChargeKind::Metadata as usize] = bytes;
        usage.additional = bytes;
        Arc::new(Self {
            usage: Mutex::new(usage),
            parent: None,
            _metadata: None,
        })
    }
    pub(crate) fn epoch(parent: &Arc<Self>) -> Result<Arc<Self>, ExtensionRefusal> {
        assert!(
            parent.parent.is_none(),
            "epoch budgets have exactly one shared parent"
        );
        let metadata = reserve(
            parent,
            ChargeKind::Metadata,
            std::mem::size_of::<Self>() + 2 * std::mem::size_of::<usize>(),
        )?;
        Ok(Arc::new(Self {
            usage: Mutex::new(Usage::default()),
            parent: Some(Arc::clone(parent)),
            _metadata: Some(metadata),
        }))
    }
    fn admit(
        &self,
        usage: &mut Usage,
        kind: ChargeKind,
        bytes: usize,
    ) -> Result<(), ExtensionRefusal> {
        let limit = match (kind, self.parent.is_some()) {
            (ChargeKind::ReliableData, true) => EPOCH_DATA,
            (ChargeKind::ReliableData, false) => GLOBAL_DATA,
            (ChargeKind::Lifecycle, true) => EPOCH_LIFECYCLE,
            (ChargeKind::Lifecycle, false) => GLOBAL_LIFECYCLE,
            (ChargeKind::Metadata, _) => METADATA,
            (ChargeKind::Projection, true) => EPOCH_PROJECTION,
            (ChargeKind::Projection, false) => projection_limit(),
            (ChargeKind::Ring, _) => 8 * 1024 * 1024,
            (ChargeKind::Snapshot, _) => 32 * 1024 * 1024,
            (ChargeKind::Viewer, _) => 8 * 1024 * 1024,
        };
        let next = usage.categories[kind as usize]
            .checked_add(bytes)
            .filter(|v| *v <= limit)
            .ok_or(ExtensionRefusal::ResourceLimit)?;
        let additional = if self.parent.is_some() {
            EPOCH_ADDITIONAL
        } else {
            additional_limit()
        };
        let extra = if kind == ChargeKind::ReliableData {
            usage.additional
        } else {
            usage
                .additional
                .checked_add(bytes)
                .filter(|v| *v <= additional)
                .ok_or(ExtensionRefusal::ResourceLimit)?
        };
        usage.categories[kind as usize] = next;
        usage.additional = extra;
        Ok(())
    }
    pub(crate) fn used(&self, kind: ChargeKind) -> usize {
        self.usage.lock().unwrap().categories[kind as usize]
    }
}
pub(crate) fn reserve(
    budget: &Arc<SliceBudget>,
    kind: ChargeKind,
    bytes: usize,
) -> Result<Reservation, ExtensionRefusal> {
    // Root-first order; rollback is under the same locks, so no partial admission is visible.
    if let Some(parent) = &budget.parent {
        let mut root = parent.usage.lock().unwrap_or_else(|e| e.into_inner());
        let mut local = budget.usage.lock().unwrap_or_else(|e| e.into_inner());
        parent.admit(&mut root, kind, bytes)?;
        if let Err(error) = budget.admit(&mut local, kind, bytes) {
            release(&mut root, kind, bytes);
            return Err(error);
        }
    } else {
        budget.admit(
            &mut budget.usage.lock().unwrap_or_else(|e| e.into_inner()),
            kind,
            bytes,
        )?;
    }
    Ok(Reservation {
        budget: Arc::clone(budget),
        kind,
        bytes,
    })
}
fn release(usage: &mut Usage, kind: ChargeKind, bytes: usize) {
    usage.categories[kind as usize] -= bytes;
    if kind != ChargeKind::ReliableData {
        usage.additional -= bytes;
    }
}
impl Reservation {
    /// Transfer an admitted increment without allocating a list of reservations.
    pub(crate) fn merge(&mut self, mut other: Self) {
        assert!(Arc::ptr_eq(&self.budget, &other.budget) && self.kind == other.kind);
        self.bytes = self.bytes.checked_add(other.bytes).expect("admitted total");
        other.bytes = 0;
    }

    pub(crate) fn allocate_bytes(self, len: usize) -> Result<ChargedBytes, ExtensionRefusal> {
        if len > self.bytes {
            return Err(ExtensionRefusal::ResourceLimit);
        }
        Ok(ChargedBytes::from_reservation(self, len))
    }
    pub(crate) fn split(&mut self, bytes: usize) -> Result<Self, ExtensionRefusal> {
        self.bytes = self
            .bytes
            .checked_sub(bytes)
            .ok_or(ExtensionRefusal::ResourceLimit)?;
        Ok(Self {
            budget: Arc::clone(&self.budget),
            kind: self.kind,
            bytes,
        })
    }
    pub(crate) fn covers(&self, budget: &Arc<SliceBudget>, kind: ChargeKind, bytes: usize) -> bool {
        Arc::ptr_eq(&self.budget, budget) && self.kind == kind && self.bytes >= bytes
    }
}
impl Drop for Reservation {
    fn drop(&mut self) {
        release(
            &mut self.budget.usage.lock().unwrap_or_else(|e| e.into_inner()),
            self.kind,
            self.bytes,
        );
        if let Some(parent) = &self.budget.parent {
            release(
                &mut parent.usage.lock().unwrap_or_else(|e| e.into_inner()),
                self.kind,
                self.bytes,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn aggregate_and_epoch_limits_are_checked_before_allocation() {
        let root = SliceBudget::new();
        let epoch = SliceBudget::epoch(&root).unwrap();
        let held = reserve(&epoch, ChargeKind::ReliableData, EPOCH_DATA).unwrap();
        assert!(matches!(
            reserve(&epoch, ChargeKind::ReliableData, 1),
            Err(ExtensionRefusal::ResourceLimit)
        ));
        assert!(matches!(
            reserve(&root, ChargeKind::ReliableData, usize::MAX),
            Err(ExtensionRefusal::ResourceLimit)
        ));
        assert!(held.allocate_bytes(EPOCH_DATA + 1).is_err());
        assert_eq!(root.used(ChargeKind::ReliableData), 0);
        assert!(reserve(&root, ChargeKind::Metadata, METADATA + 1).is_err());
        let life = reserve(&root, ChargeKind::Lifecycle, GLOBAL_LIFECYCLE).unwrap();
        assert!(reserve(&root, ChargeKind::Lifecycle, 1).is_err());
        drop(life);
        let data = reserve(&root, ChargeKind::ReliableData, GLOBAL_DATA).unwrap();
        assert!(reserve(&root, ChargeKind::ReliableData, 1).is_err());
        drop(data);
        for (kind, max) in [
            (ChargeKind::Projection, projection_limit()),
            (ChargeKind::Ring, 8 * 1024 * 1024),
            (ChargeKind::Snapshot, 32 * 1024 * 1024),
            (ChargeKind::Viewer, 8 * 1024 * 1024),
        ] {
            let held = reserve(&root, kind, max).unwrap();
            assert!(reserve(&root, kind, 1).is_err());
            drop(held);
        }
    }
}
