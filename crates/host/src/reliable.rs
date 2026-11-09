//! An epoch-local authoritative journal. Transport completion never acknowledges it.
use crate::slice_budget::{ChargeKind, EPOCH_LIFECYCLE, Reservation, SliceBudget, reserve};
use fl_host_proto::{
    ControlEvent, Cursor, ExtensionRefusal, MAX_EVENT_ENVELOPE, MAX_STDOUT_LINE, StreamId,
};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};

pub(crate) const MAX_DATA_RECORDS: usize = 4096;
pub(crate) const MAX_LIFECYCLE_RECORDS: usize = 16;
pub(crate) const MAX_CURSORS: usize = 16;
const READ_BYTES: usize = 4096;
const SLOTS: usize = MAX_DATA_RECORDS + MAX_LIFECYCLE_RECORDS;
const ARC_OVERHEAD: usize = 2 * std::mem::size_of::<usize>();
const RECORD_METADATA: usize = std::mem::size_of::<ReliableRecord>() + ARC_OVERHEAD;

pub(crate) struct ChargedBytes {
    pub(crate) terminated: bool,
    bytes: Box<[u8]>,
    len: usize,
    reservation: Reservation,
}
impl ChargedBytes {
    pub(crate) fn from_reservation(reservation: Reservation, len: usize) -> Self {
        Self {
            terminated: true,
            bytes: vec![0; len].into_boxed_slice(),
            len,
            reservation,
        }
    }
    pub(crate) fn as_mut_slice(&mut self) -> &mut [u8] {
        &mut self.bytes[..self.len]
    }
    pub(crate) fn as_slice(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
    pub(crate) fn truncate(&mut self, len: usize) {
        assert!(len <= self.len);
        self.len = len;
    }
}
pub(crate) enum ReliableBody {
    Line(ChargedBytes),
    Lifecycle(ControlEvent, Reservation),
}
pub(crate) struct ReliableRecord {
    exposed: bool,
    pub(crate) seq: u64,
    pub(crate) body: ReliableBody,
    _metadata: Reservation,
    _envelope: Option<Reservation>,
    _slot: RecordSlot,
}
struct RecordSlot {
    counts: Arc<Counts>,
    lifecycle: bool,
}
impl Drop for RecordSlot {
    fn drop(&mut self) {
        let count = if self.lifecycle {
            &self.counts.lifecycle
        } else {
            &self.counts.data
        };
        count.fetch_sub(1, Ordering::SeqCst);
    }
}
#[derive(Default)]
struct Counts {
    data: AtomicUsize,
    lifecycle: AtomicUsize,
    cursors: AtomicUsize,
    references: AtomicUsize,
}
struct Pending {
    bytes: ChargedBytes,
    offset: usize,
}
struct State {
    records: Box<[Option<Arc<ReliableRecord>>]>,
    head: usize,
    len: usize,
    next: u64,
    acked: u64,
    lifecycle_seq: [Option<u64>; 4],
    lifecycle_bytes: [Option<Reservation>; 4],
    lifecycle_metadata: [Option<Reservation>; 4],
    partial: Option<ChargedBytes>,
    pending: Option<Pending>,
    eof: bool,
    compacted: bool,
}
pub(crate) struct ReliableJournal {
    self_ref: Weak<ReliableJournal>,
    stream: StreamId,
    pty_mode: bool,
    budget: Arc<SliceBudget>,
    counts: Arc<Counts>,
    state: Mutex<State>,
    _metadata: Reservation,
}
pub(crate) struct RecordReference {
    record: Arc<ReliableRecord>,
    _metadata: Reservation,
}
impl std::ops::Deref for RecordReference {
    type Target = ReliableRecord;
    fn deref(&self) -> &Self::Target {
        &self.record
    }
}
impl Drop for RecordReference {
    fn drop(&mut self) {
        self.record
            ._slot
            .counts
            .references
            .fetch_sub(1, Ordering::SeqCst);
    }
}
pub(crate) struct ReplayCursor {
    journal: Arc<ReliableJournal>,
    next: u64,
    _metadata: Reservation,
    pub(crate) authority: Option<crate::authority::StreamGuard>,
}
impl Drop for ReplayCursor {
    fn drop(&mut self) {
        self.journal.counts.cursors.fetch_sub(1, Ordering::SeqCst);
    }
}
impl ReliableJournal {
    #[cfg(test)]
    pub(crate) fn new(
        shared: Arc<SliceBudget>,
        stream: StreamId,
    ) -> Result<Arc<Self>, ExtensionRefusal> {
        Self::new_with_mode(shared, stream, false)
    }

    pub(crate) fn new_with_mode(
        shared: Arc<SliceBudget>,
        stream: StreamId,
        pty_mode: bool,
    ) -> Result<Arc<Self>, ExtensionRefusal> {
        let budget = SliceBudget::epoch(&shared)?;
        // Fixed queue capacity has no hidden VecDeque growth. Every lifecycle kind
        // gets its worst-case envelope and node before any child can be started.
        let metadata = reserve(
            &budget,
            ChargeKind::Metadata,
            SLOTS * std::mem::size_of::<Option<Arc<ReliableRecord>>>()
                + std::mem::size_of::<Self>()
                + std::mem::size_of::<Counts>()
                // The session retains one fixed-size hosted owner for this epoch.
                + fl_host_proto::MAX_HOSTED_OWNER_BYTES + std::mem::size_of::<usize>()
                + 2 * ARC_OVERHEAD
                + 64
                + 2 * std::mem::size_of::<ChargedBytes>(),
        )?;
        let mut life = reserve(&budget, ChargeKind::Lifecycle, EPOCH_LIFECYCLE)?;
        let mut nodes = reserve(&budget, ChargeKind::Metadata, 4 * RECORD_METADATA)?;
        let lifecycle_bytes =
            std::array::from_fn(|_| Some(life.split(MAX_EVENT_ENVELOPE).expect("finite reserve")));
        let lifecycle_metadata =
            std::array::from_fn(|_| Some(nodes.split(RECORD_METADATA).expect("finite nodes")));
        Ok(Arc::new_cyclic(|weak| Self {
            self_ref: weak.clone(),
            stream,
            pty_mode,
            budget,
            counts: Arc::new(Counts::default()),
            _metadata: metadata,
            state: Mutex::new(State {
                records: (0..SLOTS)
                    .map(|_| None)
                    .collect::<Vec<_>>()
                    .into_boxed_slice(),
                head: 0,
                len: 0,
                next: 1,
                acked: 0,
                lifecycle_seq: [None; 4],
                lifecycle_bytes,
                lifecycle_metadata,
                partial: None,
                pending: None,
                eof: false,
                compacted: false,
            }),
        }))
    }
    pub(crate) fn pty_mode(&self) -> bool {
        self.pty_mode
    }
    pub(crate) fn drop_pty_unadmitted(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.partial = None;
        state.pending = None;
        state.next = state.next.saturating_add(1);
    }
    pub(crate) fn position(&self) -> (u64, u64) {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        (state.acked, state.next - 1)
    }
    pub(crate) fn budget(&self) -> &Arc<SliceBudget> {
        &self.budget
    }
    pub(crate) fn stream(&self) -> &StreamId {
        &self.stream
    }
    pub(crate) fn validate_cursor(&self, cursor: &Cursor) -> Result<(), ExtensionRefusal> {
        if cursor.stream.host != self.stream.host {
            return Err(ExtensionRefusal::WrongInstance);
        }
        if cursor.stream.session != self.stream.session || cursor.stream.epoch != self.stream.epoch
        {
            return Err(ExtensionRefusal::StaleEpoch);
        }
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if cursor.seq >= state.next {
            return Err(ExtensionRefusal::FutureCursor);
        }
        if cursor.seq < state.acked {
            return Err(ExtensionRefusal::CursorExpired);
        }
        Ok(())
    }
    pub(crate) fn allocate(&self, len: usize) -> Result<ChargedBytes, ExtensionRefusal> {
        reserve(&self.budget, ChargeKind::ReliableData, len)?.allocate_bytes(len)
    }
    pub(crate) fn prepare_read(&self) -> Result<ChargedBytes, ExtensionRefusal> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        self.drain(&mut state)?;
        if state.eof || state.pending.is_some() {
            return Err(ExtensionRefusal::ResourceLimit);
        }
        loop {
            let admission = if self.counts.data.load(Ordering::SeqCst) >= MAX_DATA_RECORDS
                && !self.coalescible(&state)
            {
                Err(ExtensionRefusal::ResourceLimit)
            } else {
                reserve(&self.budget, ChargeKind::Metadata, RECORD_METADATA)
                    .and_then(|_admission| self.allocate(READ_BYTES))
            };
            match admission {
                Err(ExtensionRefusal::ResourceLimit) if self.evict_oldest_data(&mut state) => {
                    continue;
                }
                result => return result,
            }
        }
    }
    pub(crate) fn accept_read(&self, bytes: ChargedBytes) -> Result<(), ExtensionRefusal> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.pending.is_some()
            || !bytes
                .reservation
                .covers(&self.budget, ChargeKind::ReliableData, bytes.bytes.len())
        {
            return Err(ExtensionRefusal::WrongMode);
        }
        state.pending = Some(Pending { bytes, offset: 0 });
        match self.drain(&mut state) {
            Err(ExtensionRefusal::ResourceLimit) => Ok(()), // owned pending bytes remain parked
            other => other,
        }
    }
    pub(crate) fn eof(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.eof = true;
        let _ = self.drain(&mut state); // quota saturation retains partial/pending bytes
    }
    pub(crate) fn lifecycle(&self, event: ControlEvent) -> Result<u64, ExtensionRefusal> {
        let kind = lifecycle_kind(&event)?;
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(seq) = state.lifecycle_seq[kind] {
            return Ok(seq);
        }
        let bytes = state.lifecycle_bytes[kind]
            .take()
            .ok_or(ExtensionRefusal::ResourceLimit)?;
        drop(state);
        append(self, ReliableBody::Lifecycle(event, bytes))
    }
    fn append_locked(
        &self,
        state: &mut State,
        body: &mut Option<ReliableBody>,
    ) -> Result<u64, ExtensionRefusal> {
        if state.compacted {
            return Err(ExtensionRefusal::SessionFenced);
        }
        let next = match state.next.checked_add(1) {
            Some(v) => v,
            None => return Err(ExtensionRefusal::ResourceLimit),
        };
        if let ReliableBody::Lifecycle(event, _) = body.as_mut().expect("owned admission body") {
            set_seq(event, state.next);
        }
        let kind = match body.as_ref().expect("owned admission body") {
            ReliableBody::Line(bytes) => {
                if bytes.len > MAX_STDOUT_LINE {
                    return Err(ExtensionRefusal::ResourceLimit);
                }
                if !bytes.reservation.covers(
                    &self.budget,
                    ChargeKind::ReliableData,
                    bytes.bytes.len(),
                ) {
                    return Err(ExtensionRefusal::WrongMode);
                }
                None
            }
            ReliableBody::Lifecycle(event, bytes) => {
                let kind = lifecycle_kind(event)?;
                if !bytes.covers(&self.budget, ChargeKind::Lifecycle, MAX_EVENT_ENVELOPE) {
                    return Err(ExtensionRefusal::ResourceLimit);
                }
                if let Some(seq) = state.lifecycle_seq[kind] {
                    return Ok(seq);
                }
                // Bounded counting writer prevents an uncharged serialization buffer.
                let mut counter = BoundedCounter(0);
                if serde_json::to_writer(&mut counter, event).is_err() {
                    return Err(ExtensionRefusal::ResourceLimit);
                }
                Some(kind)
            }
        };
        if kind.is_none() && self.coalescible(state) {
            let incoming = match body.as_ref().expect("body") {
                ReliableBody::Line(b) => b,
                _ => unreachable!(),
            };
            let idx = (state.head + state.len - 1) % SLOTS;
            let old = state.records[idx].as_ref().expect("tail");
            let ReliableBody::Line(previous) = &old.body else {
                unreachable!()
            };
            if let Some(len) = previous
                .len
                .checked_add(incoming.len)
                .filter(|n| *n <= MAX_STDOUT_LINE)
            {
                let mut combined = self.allocate(len)?;
                combined.terminated = incoming.terminated;
                combined.as_mut_slice()[..previous.len].copy_from_slice(previous.as_slice());
                combined.as_mut_slice()[previous.len..].copy_from_slice(incoming.as_slice());
                let envelope = self.line_envelope(&combined)?;
                let tail = Arc::get_mut(state.records[idx].as_mut().expect("tail"))
                    .expect("unexposed tail");
                tail.body = ReliableBody::Line(combined);
                tail._envelope = Some(envelope);
                body.take();
                return Ok(tail.seq);
            }
        }
        let (count, limit) = if kind.is_some() {
            (&self.counts.lifecycle, MAX_LIFECYCLE_RECORDS)
        } else {
            (&self.counts.data, MAX_DATA_RECORDS)
        };
        if state.len >= SLOTS || count.load(Ordering::SeqCst) >= limit {
            return Err(ExtensionRefusal::ResourceLimit);
        }
        let metadata = if let Some(kind) = kind {
            match state.lifecycle_metadata[kind].take() {
                Some(v) => v,
                None => return Err(ExtensionRefusal::ResourceLimit),
            }
        } else {
            reserve(&self.budget, ChargeKind::Metadata, RECORD_METADATA)?
        };
        let envelope = if let ReliableBody::Line(bytes) = body.as_ref().expect("body") {
            Some(self.line_envelope(bytes)?)
        } else {
            None
        };
        if let ReliableBody::Lifecycle(event, _) = body.as_mut().expect("owned admission body") {
            set_seq(event, state.next);
        }
        let seq = state.next;
        count.fetch_add(1, Ordering::SeqCst);
        let record = Arc::new(ReliableRecord {
            exposed: false,
            seq,
            body: body.take().expect("admitted body"),
            _metadata: metadata,
            _envelope: envelope,
            _slot: RecordSlot {
                counts: Arc::clone(&self.counts),
                lifecycle: kind.is_some(),
            },
        });
        let index = (state.head + state.len) % SLOTS;
        state.records[index] = Some(record);
        state.len += 1;
        state.next = next;
        if let Some(kind) = kind {
            state.lifecycle_seq[kind] = Some(seq);
        }
        Ok(seq)
    }
    fn coalescible(&self, state: &State) -> bool {
        if state.len == 0 {
            return false;
        }
        let tail = state.records[(state.head + state.len - 1) % SLOTS]
            .as_ref()
            .expect("tail");
        !tail.exposed
            && Arc::strong_count(tail) == 1
            && matches!(&tail.body, ReliableBody::Line(b) if !b.terminated && b.len < MAX_STDOUT_LINE)
    }
    fn line_envelope(&self, bytes: &ChargedBytes) -> Result<Reservation, ExtensionRefusal> {
        let fragments = bytes.len.max(1).div_ceil(fl_host_proto::MAX_EXTENSION_PART);
        let part = fl_host_proto::ExtensionFrame::ReliablePart {
            terminated: bytes.terminated,
            seq: u64::MAX,
            kind: fl_host_proto::ReliableKind::Line,
            more: false,
            bytes: fl_host_proto::BoundedBytes::new(Vec::new()).expect("empty"),
        };
        let mut counter = BoundedCounter(0);
        serde_json::to_writer(&mut counter, &part).map_err(|_| ExtensionRefusal::ResourceLimit)?;
        reserve(
            &self.budget,
            ChargeKind::ReliableData,
            fragments
                .checked_mul(counter.0)
                .ok_or(ExtensionRefusal::ResourceLimit)?,
        )
    }
    /// In ReliablePipe mode, only unattended data is disposable. In Pty mode, interactive
    /// output is unconditionally non-blocking and evicts oldest Line records even when
    /// observers or subscribers hold active cursors; lifecycle records remain preserved.
    fn evict_oldest_data(&self, state: &mut State) -> bool {
        if !self.pty_mode
            && (self.counts.cursors.load(Ordering::SeqCst) != 0
                || self.counts.references.load(Ordering::SeqCst) != 0)
        {
            return false;
        }
        let Some(offset) = (0..state.len).find(|offset| {
            matches!(
                state.records[(state.head + offset) % SLOTS]
                    .as_ref()
                    .map(|r| &r.body),
                Some(ReliableBody::Line(_))
            )
        }) else {
            return false;
        };
        let index = (state.head + offset) % SLOTS;
        state.records[index] = None;
        // Move only the lifecycle prefix, at most MAX_LIFECYCLE_RECORDS slots.
        for prefix in (0..offset).rev() {
            let from = (state.head + prefix) % SLOTS;
            let to = (from + 1) % SLOTS;
            state.records[to] = state.records[from].take();
        }
        state.head = (state.head + 1) % SLOTS;
        state.len -= 1;
        true
    }
    fn drain(&self, state: &mut State) -> Result<(), ExtensionRefusal> {
        loop {
            match self.drain_once(state) {
                Err(ExtensionRefusal::ResourceLimit) if self.evict_oldest_data(state) => continue,
                Err(ExtensionRefusal::ResourceLimit)
                    if self.pty_mode && (state.partial.is_some() || state.pending.is_some()) =>
                {
                    state.partial = None;
                    state.pending = None;
                    state.next = state.next.saturating_add(1);
                    return Ok(());
                }
                result => return result,
            }
        }
    }
    fn drain_once(&self, state: &mut State) -> Result<(), ExtensionRefusal> {
        loop {
            if state.partial.is_some() {
                self.publish_partial(state)?;
            }
            let Some(pending) = state.pending.as_ref() else {
                break;
            };
            let remaining = &pending.bytes.as_slice()[pending.offset..];
            if remaining.is_empty() {
                state.pending = None;
                continue;
            }
            let newline = remaining.iter().position(|b| *b == b'\n');
            let take = newline.unwrap_or(remaining.len()).min(MAX_STDOUT_LINE);
            let terminated = newline == Some(take);
            let mut fragment = self.allocate(take)?;
            fragment.terminated = terminated;
            fragment.as_mut_slice().copy_from_slice(&remaining[..take]);
            state.partial = Some(fragment);
            let pending = state.pending.as_mut().expect("pending");
            pending.offset += take + usize::from(terminated);
            if pending.offset == pending.bytes.len {
                state.pending = None;
            }
            self.publish_partial(state)?;
        }
        Ok(())
    }
    fn publish_partial(&self, state: &mut State) -> Result<(), ExtensionRefusal> {
        let Some(bytes) = state.partial.take() else {
            return Ok(());
        };
        let mut body = Some(ReliableBody::Line(bytes));
        let result = self.append_locked(state, &mut body);
        if result.is_err()
            && let Some(ReliableBody::Line(bytes)) = body
        {
            state.partial = Some(bytes);
        }
        result.map(|_| ())
    }
    pub(crate) fn is_eof(&self) -> bool {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).eof
    }
    /// Called only behind the completed-PTY authority fence after EOF and thread retirement.
    /// Dropping journal ownership never credits Ack or releases externally held buffers.
    pub(crate) fn compact_completed_pty(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        assert!(self.pty_mode && state.eof);
        state.compacted = true;
        for record in state.records.iter_mut() {
            *record = None;
        }
        state.head = 0;
        state.len = 0;
        state.partial = None;
        state.pending = None;
        state.lifecycle_bytes = std::array::from_fn(|_| None);
        state.lifecycle_metadata = std::array::from_fn(|_| None);
    }

    pub(crate) fn compacted(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .compacted
    }

    pub(crate) fn can_retire(&self) -> bool {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        self.budget.used(ChargeKind::ReliableData) == 0
            && state.len == 0
            && state.partial.is_none()
            && state.pending.is_none()
            && self.counts.data.load(Ordering::SeqCst) == 0
            && self.counts.lifecycle.load(Ordering::SeqCst) == 0
            && self.counts.cursors.load(Ordering::SeqCst) == 0
            && self.counts.references.load(Ordering::SeqCst) == 0
    }
    #[cfg(test)]
    fn retained(&self) -> usize {
        self.state.lock().unwrap().len
    }
    #[cfg(test)]
    fn live_data_records(&self) -> usize {
        self.counts.data.load(Ordering::SeqCst)
    }
}
fn lifecycle_kind(event: &ControlEvent) -> Result<usize, ExtensionRefusal> {
    match event {
        ControlEvent::Spawned { .. } => Ok(0),
        ControlEvent::Exited { .. } => Ok(1),
        ControlEvent::CleanupComplete { .. } => Ok(2),
        ControlEvent::ProtocolFailure { .. } => Ok(3),
        ControlEvent::SlowSubscriber { .. } => Err(ExtensionRefusal::WrongMode),
    }
}
fn set_seq(event: &mut ControlEvent, value: u64) {
    match event {
        ControlEvent::Spawned { seq, .. }
        | ControlEvent::Exited { seq, .. }
        | ControlEvent::CleanupComplete { seq, .. }
        | ControlEvent::ProtocolFailure { seq, .. }
        | ControlEvent::SlowSubscriber { seq, .. } => *seq = value,
    }
}
struct BoundedCounter(usize);
impl std::io::Write for BoundedCounter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self
            .0
            .checked_add(bytes.len())
            .filter(|n| *n <= MAX_EVENT_ENVELOPE)
            .ok_or_else(|| std::io::Error::other("lifecycle envelope"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
pub(crate) fn append(j: &ReliableJournal, event: ReliableBody) -> Result<u64, ExtensionRefusal> {
    let mut state = j.state.lock().unwrap_or_else(|e| e.into_inner());
    let mut body = Some(event);
    let result = j.append_locked(&mut state, &mut body);
    if result.is_err()
        && let Some(ReliableBody::Lifecycle(event, bytes)) = body
        && let Ok(kind) = lifecycle_kind(&event)
        && state.lifecycle_seq[kind].is_none()
        && state.lifecycle_bytes[kind].is_none()
    {
        state.lifecycle_bytes[kind] = Some(bytes);
    }
    result
}

pub(crate) fn begin_replay(
    j: &ReliableJournal,
    after: u64,
) -> Result<(u64, ReplayCursor), ExtensionRefusal> {
    let mut state = j.state.lock().unwrap_or_else(|e| e.into_inner());
    if state.compacted {
        return Err(ExtensionRefusal::SessionFenced);
    }
    let _ = j.drain(&mut state);
    let watermark = state.next - 1;
    if after > watermark {
        return Err(ExtensionRefusal::FutureCursor);
    }
    if after < state.acked {
        return Err(ExtensionRefusal::CursorExpired);
    }
    if j.counts.cursors.load(Ordering::SeqCst) >= MAX_CURSORS {
        return Err(ExtensionRefusal::ResourceLimit);
    }
    let metadata = reserve(
        &j.budget,
        ChargeKind::Metadata,
        std::mem::size_of::<ReplayCursor>(),
    )?;
    let next = after
        .checked_add(1)
        .ok_or(ExtensionRefusal::ResourceLimit)?;
    // An admitted cursor must never miss future bytes coalesced behind `next`.
    // Cursors that still include the tail can retain the usual coalescing benefit.
    if j.coalescible(&state) {
        let index = (state.head + state.len - 1) % SLOTS;
        let tail =
            Arc::get_mut(state.records[index].as_mut().expect("tail")).expect("unexposed tail");
        if tail.seq <= after {
            tail.exposed = true;
        }
    }
    j.counts.cursors.fetch_add(1, Ordering::SeqCst);
    Ok((
        watermark,
        ReplayCursor {
            authority: None,
            journal: j.self_ref.upgrade().ok_or(ExtensionRefusal::Io)?,
            next,
            _metadata: metadata,
        },
    ))
}
impl ReplayCursor {
    pub(crate) fn journal(&self) -> Arc<ReliableJournal> {
        Arc::clone(&self.journal)
    }
    pub(crate) fn gap(&self) -> Option<fl_host_proto::ReliableGap> {
        let state = self.journal.state.lock().unwrap_or_else(|e| e.into_inner());
        if self.next <= state.acked || self.next >= state.next {
            return None;
        }
        let next_seq = (0..state.len)
            .filter_map(|offset| state.records[(state.head + offset) % SLOTS].as_ref())
            .find(|record| record.seq >= self.next)
            .map_or(state.next, |record| record.seq);
        (next_seq > self.next).then_some(fl_host_proto::ReliableGap {
            from_seq: self.next,
            next_seq,
        })
    }
    pub(crate) fn advance_gap(
        &mut self,
        gap: fl_host_proto::ReliableGap,
    ) -> Result<(), ExtensionRefusal> {
        let current = self.gap();
        if current != Some(gap)
            && !(self.journal.pty_mode
                && current
                    .is_some_and(|c| c.from_seq == gap.from_seq && c.next_seq >= gap.next_seq))
        {
            return Err(ExtensionRefusal::CursorExpired);
        }
        self.next = gap.next_seq;
        Ok(())
    }
    pub(crate) fn next(&mut self) -> Result<Option<RecordReference>, ExtensionRefusal> {
        let mut state = self.journal.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.compacted {
            return Err(ExtensionRefusal::SessionFenced);
        }
        let _ = self.journal.drain(&mut state);
        if self.next <= state.acked {
            return Err(ExtensionRefusal::CursorExpired);
        }
        if self.next >= state.next {
            return Ok(None);
        }
        let index = (0..state.len)
            .map(|offset| (state.head + offset) % SLOTS)
            .find(|index| {
                state.records[*index]
                    .as_ref()
                    .is_some_and(|r| r.seq == self.next)
            })
            .ok_or(ExtensionRefusal::CursorExpired)?;
        if let Some(record) = state.records[index].as_mut().and_then(Arc::get_mut) {
            record.exposed = true;
        }
        let record = state.records[index]
            .as_ref()
            .ok_or(ExtensionRefusal::CursorExpired)?;
        if self.journal.counts.references.load(Ordering::SeqCst)
            >= MAX_DATA_RECORDS + MAX_LIFECYCLE_RECORDS
        {
            return Err(ExtensionRefusal::ResourceLimit);
        }
        let metadata = reserve(
            &self.journal.budget,
            ChargeKind::Metadata,
            std::mem::size_of::<RecordReference>(),
        )?;
        self.next = self
            .next
            .checked_add(1)
            .ok_or(ExtensionRefusal::ResourceLimit)?;
        self.journal
            .counts
            .references
            .fetch_add(1, Ordering::SeqCst);
        Ok(Some(RecordReference {
            record: Arc::clone(record),
            _metadata: metadata,
        }))
    }
}
pub(crate) fn acknowledge(j: &ReliableJournal, through: u64) -> Result<(), ExtensionRefusal> {
    let mut state = j.state.lock().unwrap_or_else(|e| e.into_inner());
    if state.compacted {
        return Err(ExtensionRefusal::SessionFenced);
    }
    if through >= state.next {
        return Err(ExtensionRefusal::FutureCursor);
    }
    if through <= state.acked {
        let _ = j.drain(&mut state);
        return Ok(());
    }
    while state.len > 0 {
        let head = state.head;
        if state.records[head]
            .as_ref()
            .expect("contiguous journal")
            .seq
            > through
        {
            break;
        }
        state.records[head] = None;
        state.head = (head + 1) % SLOTS;
        state.len -= 1;
    }
    state.acked = through;
    match j.drain(&mut state) {
        Err(ExtensionRefusal::ResourceLimit) => Ok(()),
        result => result,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slice_budget::EPOCH_DATA;
    fn stream() -> StreamId {
        StreamId {
            host: fl_host_proto::HostInstance::new([1; 16]),
            session: fl_host_proto::SessionId::new("a").unwrap(),
            epoch: fl_host_proto::SessionEpoch(1),
        }
    }
    fn journal() -> Arc<ReliableJournal> {
        ReliableJournal::new(SliceBudget::new(), stream()).unwrap()
    }
    fn line(j: &ReliableJournal, bytes: &[u8]) -> ReliableBody {
        let mut buf = j.allocate(bytes.len()).unwrap();
        buf.as_mut_slice().copy_from_slice(bytes);
        ReliableBody::Line(buf)
    }
    fn output_fixture(
        j: &Arc<ReliableJournal>,
    ) -> (
        Arc<crate::session::Session>,
        Arc<crate::output::OutputSession>,
    ) {
        let session = Arc::new(crate::session::Session::with_reliable(
            crate::session::SessionParts {
                session: j.stream().session.clone(),
                epoch: j.stream().epoch.0,
                vendor_pid: 0,
                vendor_start: 0,
                supervisor_pid: 0,
                supervisor_start: 0,
                pgid: 0,
                control_inode: 0,
                control: None,
                stdin: None,
            },
            Some(Arc::clone(j)),
        ));
        let output = crate::output::OutputSession::open(
            Arc::clone(&session),
            Arc::new(crate::output::Hub::new()),
        );
        (session, output)
    }
    fn admit(j: &ReliableJournal, raw: &[u8]) {
        let mut bytes = j.prepare_read().unwrap();
        bytes.as_mut_slice()[..raw.len()].copy_from_slice(raw);
        bytes.truncate(raw.len());
        j.accept_read(bytes).unwrap();
    }
    #[test]
    fn coalescing_remains_available_at_record_cap_with_live_cursor() {
        let j = journal();
        let (_, _cursor) = begin_replay(&j, 0).unwrap();
        for _ in 0..MAX_DATA_RECORDS - 1 {
            admit(&j, b"x\n");
        }
        admit(&j, b"tail");
        admit(&j, b"more");
        assert_eq!(j.live_data_records(), MAX_DATA_RECORDS);
        let state = j.state.lock().unwrap();
        let last = state.records[(state.head + state.len - 1) % SLOTS]
            .as_ref()
            .unwrap();
        assert!(matches!(&last.body, ReliableBody::Line(b) if b.as_slice()==b"tailmore"));
    }

    #[test]
    fn cursor_after_tail_receives_future_bytes_and_lf() {
        for observe_before_lf in [false, true] {
            let j = journal();
            admit(&j, b"a");
            let (watermark, mut cursor) = begin_replay(&j, 1).unwrap();
            assert_eq!(watermark, 1);
            admit(&j, b"b");
            let mut got = Vec::new();
            if observe_before_lf {
                let record = cursor.next().unwrap().expect("new bytes after attach");
                assert_eq!(record.seq, 2);
                let ReliableBody::Line(bytes) = &record.body else {
                    panic!("line")
                };
                assert!(!bytes.terminated);
                got.extend_from_slice(bytes.as_slice());
            }
            admit(&j, b"\n");
            while let Some(record) = cursor.next().unwrap() {
                let ReliableBody::Line(bytes) = &record.body else {
                    panic!("line")
                };
                got.extend_from_slice(bytes.as_slice());
                if bytes.terminated {
                    got.push(b'\n');
                }
            }
            assert_eq!(got, b"b\n");
        }
    }

    #[test]
    fn cursor_before_unobserved_tail_preserves_coalescing() {
        let j = journal();
        admit(&j, b"a");
        let (_, mut cursor) = begin_replay(&j, 0).unwrap();
        admit(&j, b"b\n");
        let record = cursor.next().unwrap().unwrap();
        assert_eq!(record.seq, 1);
        assert!(matches!(&record.body, ReliableBody::Line(b)
            if b.as_slice() == b"ab" && b.terminated));
        assert!(cursor.next().unwrap().is_none());
    }

    #[test]
    fn tiny_reads_coalesce_and_exposed_tail_never_changes() {
        let j = journal();
        let mut expected = Vec::new();
        for i in 0..100_000 {
            let raw: &[u8] = if i % 1000 == 999 { b"x\n" } else { b"x" };
            admit(&j, raw);
            expected.extend_from_slice(raw);
        }
        assert_eq!(j.live_data_records(), 100);
        let (_, mut cursor) = begin_replay(&j, 0).unwrap();
        let mut got = Vec::new();
        while let Some(record) = cursor.next().unwrap() {
            let ReliableBody::Line(bytes) = &record.body else {
                panic!("line")
            };
            got.extend_from_slice(bytes.as_slice());
            if bytes.terminated {
                got.push(b'\n');
            }
        }
        assert_eq!(got, expected);
        drop(cursor);
        let k = journal();
        admit(&k, b"a");
        let (_, mut cursor) = begin_replay(&k, 0).unwrap();
        drop(cursor.next().unwrap().unwrap());
        drop(cursor);
        admit(&k, b"b");
        let (_, mut cursor) = begin_replay(&k, 0).unwrap();
        assert!(
            matches!(&cursor.next().unwrap().unwrap().body, ReliableBody::Line(b) if b.as_slice()==b"a")
        );
        assert!(
            matches!(&cursor.next().unwrap().unwrap().body, ReliableBody::Line(b) if b.as_slice()==b"b")
        );
    }
    #[test]
    fn unattended_eviction_preserves_lifecycle_and_reports_exact_gap() {
        let j = journal();
        j.lifecycle(ControlEvent::CleanupComplete {
            session: stream().session,
            session_epoch: stream().epoch,
            seq: 0,
        })
        .unwrap();
        for i in 0..100_000 {
            admit(&j, format!("{i}\n").as_bytes());
        }
        assert_eq!(j.position().0, 0);
        let (_, mut cursor) = begin_replay(&j, 0).unwrap();
        assert!(cursor.gap().is_none());
        assert!(matches!(
            cursor.next().unwrap().unwrap().body,
            ReliableBody::Lifecycle(..)
        ));
        let gap = cursor.gap().expect("data loss notice");
        assert_eq!(gap.from_seq, 2);
        assert_eq!(gap.next_seq, 100_002 - MAX_DATA_RECORDS as u64);
        assert_eq!(
            cursor.gap(),
            Some(gap),
            "notice is not consumed before write success"
        );
        cursor.advance_gap(gap).unwrap();
        for i in 100_000 - MAX_DATA_RECORDS..100_000 {
            let r = cursor.next().unwrap().unwrap();
            assert!(
                matches!(&r.body, ReliableBody::Line(b) if b.terminated && b.as_slice()==i.to_string().as_bytes())
            );
        }
        assert!(cursor.next().unwrap().is_none());
    }
    #[test]
    fn unattended_byte_pressure_and_live_cursor_backpressure_are_distinct() {
        let j = journal();
        for _ in 0..100_000 {
            admit(&j, &[b'z'; 128]);
        }
        assert!(j.budget.used(ChargeKind::ReliableData) <= EPOCH_DATA);
        assert_eq!(j.position().0, 0);
        let (_, mut cursor) = begin_replay(&j, 0).unwrap();
        assert!(cursor.gap().is_some());
        let gap = cursor.gap().unwrap();
        cursor.advance_gap(gap).unwrap();
        while let Some(r) = cursor.next().unwrap() {
            assert!(
                matches!(&r.body, ReliableBody::Line(b) if !b.terminated && b.as_slice().iter().all(|x| *x==b'z'))
            );
        }
        let k = journal();
        let (_, _live) = begin_replay(&k, 0).unwrap();
        for _ in 0..MAX_DATA_RECORDS {
            admit(&k, b"x\n");
        }
        assert!(k.prepare_read().is_err());
        assert_eq!(k.live_data_records(), MAX_DATA_RECORDS);
        assert_eq!(k.position(), (0, MAX_DATA_RECORDS as u64));
    }

    #[test]
    fn unattended_record_pressure_drops_oldest_without_ack() {
        let j = journal();
        for _ in 0..100_000 {
            let mut bytes = j.prepare_read().expect("unattended producer remains live");
            bytes.as_mut_slice()[..2].copy_from_slice(b"x\n");
            bytes.truncate(2);
            j.accept_read(bytes).unwrap();
        }
        assert_eq!(j.position().0, 0, "eviction is not acknowledgement");
        assert!(j.live_data_records() <= MAX_DATA_RECORDS);
        assert!(j.budget.used(ChargeKind::ReliableData) <= EPOCH_DATA);
    }

    #[test]
    fn arbitrarily_long_unterminated_output_is_not_a_protocol_failure() {
        let j = journal();
        for _ in 0..513 {
            let mut bytes = j.prepare_read().unwrap();
            bytes.as_mut_slice().fill(b'x');
            assert_eq!(j.accept_read(bytes), Ok(()));
        }
        j.eof();
        let (_, mut cursor) = begin_replay(&j, 0).unwrap();
        let mut count = 0;
        while let Some(record) = cursor.next().unwrap() {
            if let ReliableBody::Line(bytes) = &record.body {
                count += bytes.as_slice().len();
                assert!(bytes.as_slice().iter().all(|b| *b == b'x'));
            }
        }
        assert_eq!(count, 513 * 4096);
    }

    #[test]
    fn real_sink_driver_retains_pending_read_at_saturation_and_stop() {
        let j = journal();
        let (_watermark, _active) = begin_replay(&j, 0).unwrap();
        for _ in 0..MAX_DATA_RECORDS - 1 {
            append(&j, line(&j, b"previous")).unwrap();
        }
        let (session, output) = output_fixture(&j);
        let mut reads = 0;
        crate::session::pump_reliable_stdout_with(&session, output.as_ref(), &j, |buffer| {
            reads += 1;
            assert_eq!(reads, 1, "no read after cancellation");
            buffer[..11].copy_from_slice(b"held\nafter\n");
            session.stop_pumps();
            Ok(11)
        });
        assert!(j.is_eof());
        assert_eq!(j.retained(), MAX_DATA_RECORDS);
        assert!(j.state.lock().unwrap().partial.is_some());
        acknowledge(&j, MAX_DATA_RECORDS as u64).unwrap();
        let (_, mut cursor) = begin_replay(&j, MAX_DATA_RECORDS as u64).unwrap();
        let record = cursor.next().unwrap().unwrap();
        assert!(matches!(&record.body,ReliableBody::Line(bytes) if bytes.as_slice()==b"after"));
        assert!(cursor.next().unwrap().is_none());
        assert!(!j.can_retire());
    }
    #[test]
    fn real_sink_driver_assembles_lines_and_eof_without_legacy_copy() {
        let j = journal();
        let (session, output) = output_fixture(&j);
        let chunks: [&[u8]; 3] = [b"one\nt", b"wo", b""];
        let mut chunks = chunks.into_iter();
        crate::session::pump_reliable_stdout_with(&session, output.as_ref(), &j, |buffer| {
            let bytes = chunks.next().expect("bounded reads");
            buffer[..bytes.len()].copy_from_slice(bytes);
            Ok(bytes.len())
        });
        let (_, mut cursor) = begin_replay(&j, 0).unwrap();
        for expected in [b"one".as_slice(), b"two".as_slice()] {
            let record = cursor.next().unwrap().unwrap();
            assert!(matches!(&record.body,ReliableBody::Line(bytes) if bytes.as_slice()==expected));
        }
        assert!(cursor.next().unwrap().is_none());
    }
    #[test]
    fn parked_reference_cap_is_charged_before_reference_creation() {
        let j = journal();
        for _ in 0..MAX_DATA_RECORDS {
            append(&j, line(&j, b"x")).unwrap();
        }
        let (_, mut first) = begin_replay(&j, 0).unwrap();
        let mut parked = Vec::new();
        while let Some(record) = first.next().unwrap() {
            parked.push(record);
        }
        let (_, mut second) = begin_replay(&j, 0).unwrap();
        for _ in 0..MAX_LIFECYCLE_RECORDS {
            parked.push(second.next().unwrap().unwrap());
        }
        assert!(matches!(
            second.next(),
            Err(ExtensionRefusal::ResourceLimit)
        ));
        parked.pop();
        assert!(second.next().unwrap().is_some());
    }
    #[test]
    fn eof_pending_resumes_when_last_parked_reference_releases_capacity() {
        let j = journal();
        for _ in 0..MAX_DATA_RECORDS {
            append(&j, line(&j, b"x")).unwrap();
        }
        let (_, mut cursor) = begin_replay(&j, 0).unwrap();
        let mut parked = Vec::new();
        while let Some(record) = cursor.next().unwrap() {
            parked.push(record);
        }
        let mut bytes = j.allocate(6).unwrap();
        bytes.as_mut_slice().copy_from_slice(b"after\n");
        j.state.lock().unwrap().pending = Some(Pending { bytes, offset: 0 });
        j.eof();
        acknowledge(&j, MAX_DATA_RECORDS as u64).unwrap();
        assert!(cursor.next().unwrap().is_none());
        parked.pop();
        let record = cursor.next().unwrap().unwrap();
        assert!(matches!(&record.body,ReliableBody::Line(bytes) if bytes.as_slice()==b"after"));
    }
    #[test]
    fn completed_partial_is_retried_before_next_pending_line() {
        let j = journal();
        let mut state = j.state.lock().unwrap();
        state.partial = Some(j.allocate(1).unwrap());
        state.partial.as_mut().unwrap().as_mut_slice()[0] = b'a';
        let mut bytes = j.allocate(2).unwrap();
        bytes.as_mut_slice().copy_from_slice(b"b\n");
        state.pending = Some(Pending { bytes, offset: 0 });
        j.drain(&mut state).unwrap();
        drop(state);
        let (_, mut cursor) = begin_replay(&j, 0).unwrap();
        for expected in [b"a".as_slice(), b"b".as_slice()] {
            assert!(
                matches!(&cursor.next().unwrap().unwrap().body,ReliableBody::Line(bytes) if bytes.as_slice()==expected)
            );
        }
    }
    #[test]
    fn cursor_is_bound_to_actual_instance_and_epoch() {
        let j = journal();
        let mut cursor = Cursor {
            stream: stream(),
            seq: 0,
        };
        assert!(j.validate_cursor(&cursor).is_ok());
        cursor.stream.host = fl_host_proto::HostInstance::new([2; 16]);
        assert_eq!(
            j.validate_cursor(&cursor),
            Err(ExtensionRefusal::WrongInstance)
        );
        cursor.stream = stream();
        cursor.stream.epoch = fl_host_proto::SessionEpoch(2);
        assert_eq!(
            j.validate_cursor(&cursor),
            Err(ExtensionRefusal::StaleEpoch)
        );
    }
    #[test]
    fn socket_write_does_not_retire_reliable_record() {
        let j = journal();
        append(&j, line(&j, b"one")).unwrap();
        let (_, mut cursor) = begin_replay(&j, 0).unwrap();
        let record = cursor.next().unwrap().unwrap();
        assert_eq!(record.seq, 1);
        assert!(matches!(&record.body,ReliableBody::Line(bytes) if bytes.as_slice()==b"one"));
        drop(record); // complete socket write is just reference release
        assert_eq!(j.retained(), 1);
        assert_eq!(acknowledge(&j, 2), Err(ExtensionRefusal::FutureCursor));
        acknowledge(&j, 1).unwrap();
        assert_eq!(j.retained(), 0);
    }
    #[test]
    fn snapshot_watermark_has_no_gap_or_duplicate() {
        let j = journal();
        append(&j, line(&j, b"a")).unwrap();
        let (w, mut cursor) = begin_replay(&j, 0).unwrap();
        assert_eq!(w, 1);
        append(&j, line(&j, b"b")).unwrap();
        assert_eq!(cursor.next().unwrap().unwrap().seq, 1);
        assert_eq!(cursor.next().unwrap().unwrap().seq, 2);
        assert!(cursor.next().unwrap().is_none());
        assert!(matches!(
            begin_replay(&j, 3),
            Err(ExtensionRefusal::FutureCursor)
        ));
    }
    #[test]
    fn capacity_waits_for_last_reference() {
        let j = journal();
        for _ in 0..MAX_DATA_RECORDS {
            append(&j, line(&j, b"x")).unwrap();
        }
        let (_, mut cursor) = begin_replay(&j, 0).unwrap();
        let parked = cursor.next().unwrap().unwrap();
        acknowledge(&j, MAX_DATA_RECORDS as u64).unwrap();
        assert_eq!(j.live_data_records(), 1);
        drop(cursor);
        assert!(!j.can_retire());
        drop(parked);
        assert!(j.can_retire());
        assert_eq!(j.live_data_records(), 0);
    }
    #[test]
    fn reliable_saturation_preserves_lifecycle() {
        let j = journal();
        let _held = j.allocate(EPOCH_DATA).unwrap();
        assert!(j.allocate(1).is_err());
        let event = ControlEvent::CleanupComplete {
            session: fl_host_proto::SessionId::new("a").unwrap(),
            session_epoch: fl_host_proto::SessionEpoch(1),
            seq: 0,
        };
        assert_eq!(j.lifecycle(event.clone()).unwrap(), 1);
        assert_eq!(j.lifecycle(event).unwrap(), 1);
        let second = journal();
        assert!(append(&second, line(&second, b"other")).is_ok());
    }
    #[test]
    fn every_finite_lifecycle_transition_has_independent_one_shot_capacity() {
        use crate::session::StdoutSink;
        let j = journal();
        let (session, output) = output_fixture(&j);
        let _saturation = j.allocate(EPOCH_DATA).unwrap();
        for _ in 0..2 {
            output.emit_spawned(fl_host_proto::HostedChild {
                state: session.state(),
            });
            output.on_leader(fl_host_proto::ChildExit {
                code: Some(259),
                signal: None,
            });
            output.on_cleanup(true);
            assert_eq!(
                j.lifecycle(ControlEvent::ProtocolFailure {
                    session: stream().session,
                    session_epoch: stream().epoch,
                    seq: 0,
                    reason: "bounded failure".into()
                })
                .unwrap(),
                4
            );
        }
        assert_eq!(j.retained(), 4);
        assert_eq!(j.counts.lifecycle.load(Ordering::SeqCst), 4);
        assert_eq!(
            j.lifecycle(ControlEvent::SlowSubscriber {
                session: stream().session,
                session_epoch: stream().epoch,
                seq: 0
            }),
            Err(ExtensionRefusal::WrongMode)
        );
        let (_, mut cursor) = begin_replay(&j, 0).unwrap();
        for expected in 1..=4 {
            assert_eq!(cursor.next().unwrap().unwrap().seq, expected);
        }
        assert!(cursor.next().unwrap().is_none());
    }
    #[test]
    fn envelope_refusal_restores_lifecycle_reserve_and_does_not_consume_sequence() {
        let j = journal();
        let event = |reason| ControlEvent::ProtocolFailure {
            session: stream().session,
            session_epoch: stream().epoch,
            seq: 0,
            reason,
        };
        assert_eq!(
            j.lifecycle(event("x".repeat(MAX_EVENT_ENVELOPE))),
            Err(ExtensionRefusal::ResourceLimit)
        );
        assert_eq!(j.lifecycle(event("bounded".into())).unwrap(), 1);
    }
    #[test]
    fn journal_limits_checked_before_read() {
        let j = journal();
        let (_watermark, _active) = begin_replay(&j, 0).unwrap();
        for _ in 0..MAX_DATA_RECORDS {
            append(&j, line(&j, b"x")).unwrap();
        }
        assert!(j.prepare_read().is_err());
        let mut cursors = Vec::new();
        for _ in 1..MAX_CURSORS {
            cursors.push(begin_replay(&j, 0).unwrap().1);
        }
        assert!(matches!(
            begin_replay(&j, 0),
            Err(ExtensionRefusal::ResourceLimit)
        ));
        drop(cursors);
        j.state.lock().unwrap().next = u64::MAX;
        assert_eq!(
            append(&j, line(&j, b"x")),
            Err(ExtensionRefusal::ResourceLimit)
        );
    }
    #[test]
    fn pty_mode_never_blocks_with_stalled_cursor_and_reports_exact_gap() {
        let j = ReliableJournal::new_with_mode(SliceBudget::new(), stream(), true).unwrap();
        assert!(j.pty_mode());
        let spawned = ControlEvent::Spawned {
            session: stream().session,
            session_epoch: stream().epoch,
            child: fl_host_proto::HostedChild {
                state: fl_host_proto::ChildState::Running(fl_host_proto::ChildRecord {
                    session: stream().session,
                    session_epoch: stream().epoch,
                    kind: fl_host_proto::SpawnKind::Pty,
                    pid: 42,
                    input: fl_host_proto::InputState::Writable,
                    started_at_ms: 1,
                    exit: None,
                    consumer_stalled: false,
                }),
            },
            seq: 0,
        };
        assert_eq!(j.lifecycle(spawned).unwrap(), 1);
        let (_, mut stalled) = begin_replay(&j, 0).unwrap();
        let first = stalled.next().unwrap().unwrap();
        assert_eq!(first.seq, 1);

        // 1. Flood > MAX_DATA_RECORDS lines while `stalled` cursor is open and holding `first`.
        for i in 0..10_000 {
            let line = format!("line-{i}\n");
            let mut buf = j
                .prepare_read()
                .expect("pty_mode never stalls on record count");
            buf.as_mut_slice()[..line.len()].copy_from_slice(line.as_bytes());
            buf.truncate(line.len());
            j.accept_read(buf).unwrap();
        }
        assert!(j.live_data_records() <= MAX_DATA_RECORDS);
        assert!(j.budget.used(ChargeKind::ReliableData) <= EPOCH_DATA);

        // 2. Flood > 8 MiB of unterminated > 1 MiB output while `stalled` cursor remains open.
        for _ in 0..2200 {
            let mut buf = j
                .prepare_read()
                .expect("pty_mode never stalls on byte budget");
            buf.as_mut_slice().fill(b'p');
            j.accept_read(buf).unwrap();
        }
        let mut tail = j.prepare_read().unwrap();
        tail.as_mut_slice()[..5].copy_from_slice(b"\ndone");
        tail.truncate(5);
        j.accept_read(tail).unwrap();

        let gap = stalled.gap().expect("evicted data must produce gap");
        assert_eq!(gap.from_seq, 2);
        assert!(gap.next_seq > 10_000);
        stalled.advance_gap(gap).unwrap();
        let mut last = None;
        while let Some(rec) = stalled.next().unwrap() {
            last = Some(rec);
        }
        let last = last.expect("retained tail record");
        assert!(matches!(
            &last.body,
            ReliableBody::Line(bytes) if !bytes.terminated && bytes.as_slice() == b"done"
        ));
    }
}
