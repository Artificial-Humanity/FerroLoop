//! Bounded completed-PTY diagnostic history. Inventory remains in HostInner outputs.
use crate::slice_budget::{ChargeKind, Reservation, SliceBudget, reserve};
use fl_host_proto::{ExtensionRefusal, StreamId};
use std::sync::Arc;

pub(crate) const COMPLETED_PTY_LIMIT: usize = 8;
pub(crate) const RECENT_EVICTIONS: usize = 64;

pub(crate) struct Retention {
    recent: [Option<StreamId>; RECENT_EVICTIONS],
    next: usize,
    pub(crate) evicted_total: u64,
    _metadata: Reservation,
}
impl Retention {
    pub(crate) fn new(budget: &Arc<SliceBudget>) -> Result<Self, ExtensionRefusal> {
        let metadata = reserve(
            budget,
            ChargeKind::Metadata,
            std::mem::size_of::<Self>() + RECENT_EVICTIONS * 64,
        )?;
        Ok(Self {
            recent: std::array::from_fn(|_| None),
            next: 0,
            evicted_total: 0,
            _metadata: metadata,
        })
    }

    pub(crate) fn evict(&mut self, stream: StreamId) {
        // HostInstance's Debug is deliberately redacted; serialize the public
        // stream identity so operators can identify the actual evicted epoch.
        eprintln!(
            "fl-host: completed PTY evicted {}",
            serde_json::to_string(&stream).expect("stream identity")
        );
        self.recent[self.next] = Some(stream);
        self.next = (self.next + 1) % RECENT_EVICTIONS;
        self.evicted_total = self.evicted_total.saturating_add(1);
    }

    // Callers must pre-admit response storage before requesting a snapshot.
    pub(crate) fn recent(&self) -> Vec<StreamId> {
        let mut recent = Vec::with_capacity(RECENT_EVICTIONS);
        for offset in 0..RECENT_EVICTIONS {
            if let Some(stream) = &self.recent[(self.next + offset) % RECENT_EVICTIONS] {
                recent.push(stream.clone());
            }
        }
        recent
    }
}
