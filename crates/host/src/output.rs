//! Shared stdout backlog and bounded event subscribers.
//!
//! Storage is charged once. Subscribers hold references. A socket write is not
//! application consumption, and a dropped subscriber does not discard bytes that
//! nobody has fully written.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use fl_host_proto::{
    ChildExit, ControlEvent, Frame, FrameKind, HostedChild, LINE_HEADER_LEN, MAX_EVENT_ENVELOPE,
    MAX_FRAME_PAYLOAD, MAX_STDOUT_BACKLOG, MAX_STDOUT_LINE, MAX_SUBSCRIBERS, SessionEpoch,
    SessionId, ShutdownSwitch, fragment_line_with_termination,
};

use crate::input::{Barrier, hold_dir, poll_barrier};
use crate::session::{Session, StdoutSink};

const MAX_EVENTS: usize = 4096;
const SUB_QUEUE: usize = 64;
const SUB_BYTES: usize = 2 * 1024 * 1024;
const TERMINAL_SLOTS: usize = 64;

fn line_envelope(len: usize) -> usize {
    if len == 0 {
        return LINE_HEADER_LEN;
    }
    let chunk = MAX_FRAME_PAYLOAD - LINE_HEADER_LEN;
    len.div_ceil(chunk).saturating_mul(LINE_HEADER_LEN)
}

fn control_len(event: &ControlEvent) -> usize {
    serde_json::to_vec(event)
        .map(|bytes| bytes.len().clamp(1, MAX_EVENT_ENVELOPE))
        .unwrap_or(1)
}

/// One session epoch. `seq` restarts on respawn, so the cursor is not per name alone.
type StreamKey = (String, u64);

enum Body {
    Line(Vec<u8>),
    Control(ControlEvent),
}

struct Track {
    holders: usize,
    completes: usize,
    retired: bool,
    delivered: HashSet<u64>,
}

struct Logical {
    terminated: bool,
    exposed: AtomicBool,
    index: u64,
    seq: u64,
    epoch: u64,
    raw_len: usize,
    envelope: usize,
    session: String,
    exited: bool,
    body: Body,
    track: Mutex<Track>,
    hub: std::sync::Weak<Hub>,
}

pub(crate) struct SubscriberSlot(Arc<AtomicUsize>);
impl Drop for SubscriberSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

pub struct Hub {
    fanout: Mutex<Fanout>,
    sessions: Mutex<Vec<std::sync::Weak<OutputSession>>>,
    index: AtomicU64,
    status: Option<PathBuf>,
    status_lock: Mutex<()>,
    slots: Arc<AtomicUsize>,
}

struct Fanout {
    subs: Vec<Sub>,
    next_id: u64,
}

struct Sub {
    id: u64,
    filter: Option<String>,
    subscribed_at: u64,
    queue: VecDeque<Arc<Logical>>,
    queued_bytes: usize,
    terminals: HashMap<String, Arc<Logical>>,
    dropped: bool,
    slow: bool,
    wake: Arc<Condvar>,
    kill: Option<Arc<ShutdownSwitch>>,
    /// Next `seq` to enqueue. Excess catchup stays in shared pending storage.
    cursors: HashMap<StreamKey, u64>,
}

pub struct OutputSession {
    session: Arc<Session>,
    reliable: Option<Arc<crate::reliable::ReliableJournal>>,
    hub: Arc<Hub>,
    inner: Mutex<OutInner>,
    cv: Condvar,
    /// Held from `seq` and fanout-index allocation through publish.
    order: Mutex<()>,
    seq: AtomicU64,
    stop: AtomicBool,
    stall_noted: AtomicBool,
    fence: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

struct OutInner {
    events: Vec<Arc<Logical>>,
    staging: Vec<u8>,
    eof: bool,
}

struct Charge {
    events: usize,
    charge: usize,
    raw: usize,
}

impl Hub {
    pub fn new() -> Self {
        Self {
            fanout: Mutex::new(Fanout {
                subs: Vec::new(),
                next_id: 1,
            }),
            sessions: Mutex::new(Vec::new()),
            index: AtomicU64::new(1),
            status: std::env::var_os("FL_HOST_BACKLOG_STATUS").map(PathBuf::from),
            status_lock: Mutex::new(()),
            slots: Arc::new(AtomicUsize::new(0)),
        }
    }

    #[allow(deprecated)]
    pub(crate) fn reserve_slot(&self) -> Result<SubscriberSlot, fl_host_proto::ExtensionRefusal> {
        self.slots
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                n.checked_add(1).filter(|n| *n <= MAX_SUBSCRIBERS)
            })
            .map_err(|_| fl_host_proto::ExtensionRefusal::ResourceLimit)?;
        Ok(SubscriberSlot(Arc::clone(&self.slots)))
    }

    pub fn subscribe(
        &self,
        filter: Option<SessionId>,
        kill: Option<Arc<ShutdownSwitch>>,
    ) -> Result<Subscriber, (Option<Arc<ShutdownSwitch>>, &'static str)> {
        let slot = match self.reserve_slot() {
            Ok(slot) => slot,
            Err(_) => return Err((kill, "subscriber limit")),
        };
        let mut fanout = self.fanout.lock().unwrap_or_else(|err| err.into_inner());
        fanout.subs.retain(|sub| !sub.dropped);
        let alive = fanout.subs.len();
        if alive >= MAX_SUBSCRIBERS {
            return Err((kill, "subscriber limit"));
        }
        let id = fanout.next_id;
        fanout.next_id = fanout.next_id.saturating_add(1);
        let subscribed_at = self.index.load(Ordering::SeqCst);
        let wake = Arc::new(Condvar::new());
        let lock = Arc::new(Mutex::new(()));
        fanout.subs.push(Sub {
            id,
            filter: filter.map(|id| id.as_str().to_owned()),
            subscribed_at,
            queue: VecDeque::new(),
            queued_bytes: 0,
            terminals: HashMap::new(),
            dropped: false,
            slow: false,
            wake: Arc::clone(&wake),
            kill,
            cursors: HashMap::new(),
        });
        drop(fanout);
        self.pull_preexisting();
        Ok(Subscriber {
            id,
            wake,
            lock,
            _slot: slot,
        })
    }

    fn pull_preexisting(&self) {
        for session in self.live_sessions() {
            session.deliver_pending();
        }
    }

    fn live_sessions(&self) -> Vec<Arc<OutputSession>> {
        let mut slots = self.sessions.lock().unwrap_or_else(|err| err.into_inner());
        slots.retain(|slot| slot.strong_count() > 0);
        slots.iter().filter_map(|slot| slot.upgrade()).collect()
    }

    /// Caller holds the session `order` lock. Enqueue only the next per-epoch `seq`.
    fn admit(&self, pending: &[Arc<Logical>], key: &StreamKey, next_seq: u64) {
        let mut fanout = self.fanout.lock().unwrap_or_else(|err| err.into_inner());
        for sub in fanout.subs.iter_mut() {
            if sub.dropped || !sub.wants_key(key) {
                continue;
            }
            prime_cursor(sub, key, pending, next_seq);
            drive_cursor(sub, key, pending, next_seq);
        }
        for sub in &fanout.subs {
            sub.wake.notify_all();
        }
    }

    pub(crate) fn forget_epoch(&self, key: &StreamKey) {
        let mut fanout = self.fanout.lock().unwrap_or_else(|err| err.into_inner());
        for sub in &mut fanout.subs {
            sub.cursors.remove(key);
        }
    }

    fn all_backlogged(&self, key: &StreamKey) -> bool {
        let fanout = self.fanout.lock().unwrap_or_else(|err| err.into_inner());
        let mut live = fanout
            .subs
            .iter()
            .filter(|sub| !sub.dropped && !sub.slow && sub.wants_key(key))
            .peekable();
        live.peek().is_some()
            && live.all(|sub| sub.queue.len() >= SUB_QUEUE / 2 || sub.queued_bytes >= SUB_BYTES / 2)
    }

    fn note_status(&self, charge: Charge) {
        let Some(path) = &self.status else {
            return;
        };
        let _guard = self
            .status_lock
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let line = format!(
            "events={} charge={} raw={}\n",
            charge.events, charge.charge, charge.raw
        );
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            use std::io::Write;
            let _ = file.write_all(line.as_bytes());
        }
    }

    pub fn shutdown(&self) {
        let released = {
            let mut fanout = self.fanout.lock().unwrap_or_else(|err| err.into_inner());
            let ids: Vec<_> = fanout.subs.iter().map(|sub| sub.id).collect();
            let mut released = Vec::new();
            for id in ids {
                released.extend(take_sub(&mut fanout, id));
            }
            released
        };
        for event in released {
            settle(&event, false);
        }
    }
}

pub struct Subscriber {
    _slot: SubscriberSlot,
    id: u64,
    wake: Arc<Condvar>,
    lock: Arc<Mutex<()>>,
}

fn assign(sub: &mut Sub, event: &Arc<Logical>, preexisting: bool) -> Assign {
    if sub.dropped {
        return Assign::Skip;
    }
    let mut track = event.track.lock().unwrap_or_else(|err| err.into_inner());
    if track.retired || !track.delivered.insert(sub.id) {
        return Assign::Skip;
    }
    let bytes = event.raw_len.saturating_add(event.envelope);
    if event.exited {
        if sub.terminals.contains_key(&event.session) || sub.terminals.len() >= TERMINAL_SLOTS {
            track.delivered.remove(&sub.id);
            return Assign::Overflow;
        }
        event.exposed.store(true, Ordering::SeqCst);
        track.holders = track.holders.saturating_add(1);
        sub.terminals
            .insert(event.session.clone(), Arc::clone(event));
        return Assign::Queued;
    }
    if sub.queue.len() >= SUB_QUEUE || sub.queued_bytes.saturating_add(bytes) > SUB_BYTES {
        track.delivered.remove(&sub.id);
        return if preexisting {
            Assign::Skip
        } else {
            Assign::Overflow
        };
    }
    event.exposed.store(true, Ordering::SeqCst);
    track.holders = track.holders.saturating_add(1);
    sub.queued_bytes = sub.queued_bytes.saturating_add(bytes);
    sub.queue.push_back(Arc::clone(event));
    Assign::Queued
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Assign {
    Queued,
    Skip,
    Overflow,
}

fn prime_cursor(sub: &mut Sub, key: &StreamKey, pending: &[Arc<Logical>], next_seq: u64) {
    if sub.cursors.contains_key(key) {
        return;
    }
    // An event another subscriber still holds is unfinished. Starting past it
    // would drop the catch-up once that holder releases it.
    let mut start = next_seq;
    for event in pending {
        if event.session != key.0 || event.epoch != key.1 || event.seq >= start {
            continue;
        }
        let track = event.track.lock().unwrap_or_else(|err| err.into_inner());
        if !track.retired && track.completes == 0 && !track.delivered.contains(&sub.id) {
            start = event.seq;
        }
    }
    sub.cursors.insert(key.clone(), start);
}

/// Queue a prefix of this epoch. Stop when the next `seq` is missing or the budget is full.
fn drive_cursor(sub: &mut Sub, key: &StreamKey, pending: &[Arc<Logical>], next_seq: u64) {
    loop {
        if sub.dropped {
            return;
        }
        let Some(cursor) = sub.cursors.get(key).copied() else {
            return;
        };
        let event = pending
            .iter()
            .find(|event| event.session == key.0 && event.epoch == key.1 && event.seq == cursor)
            .cloned();
        let Some(event) = event else {
            if cursor < next_seq {
                let Some(next) = cursor.checked_add(1) else {
                    return;
                };
                sub.cursors.insert(key.clone(), next);
                continue;
            }
            return;
        };
        if !offer(sub, key, cursor, &event) {
            return;
        }
    }
}

/// `true` when the cursor advanced.
fn offer(sub: &mut Sub, key: &StreamKey, cursor: u64, event: &Arc<Logical>) -> bool {
    let preexisting = event.index < sub.subscribed_at;
    match assign(sub, event, preexisting) {
        Assign::Queued => {
            let Some(next) = cursor.checked_add(1) else {
                return false;
            };
            sub.cursors.insert(key.clone(), next);
            true
        }
        Assign::Overflow => {
            sub.slow = true;
            false
        }
        Assign::Skip => {
            let already = event
                .track
                .lock()
                .unwrap_or_else(|err| err.into_inner())
                .delivered
                .contains(&sub.id);
            if already {
                let Some(next) = cursor.checked_add(1) else {
                    return false;
                };
                sub.cursors.insert(key.clone(), next);
                true
            } else {
                false
            }
        }
    }
}

fn take_sub(fanout: &mut Fanout, id: u64) -> Vec<Arc<Logical>> {
    let Some(sub) = fanout.subs.iter_mut().find(|sub| sub.id == id) else {
        return Vec::new();
    };
    if sub.dropped {
        return Vec::new();
    }
    sub.dropped = true;
    sub.slow = true;
    sub.cursors.clear();
    if let Some(kill) = sub.kill.take() {
        kill.trip();
    }
    let mut events: Vec<_> = sub.queue.drain(..).collect();
    sub.queued_bytes = 0;
    events.extend(sub.terminals.drain().map(|(_, event)| event));
    sub.wake.notify_all();
    events
}

fn settle(event: &Logical, wrote: bool) {
    // Serialize retirement with admission so catchup cursor obligations cannot
    // race retirement of the shared pending event.
    if let Some(hub) = event.hub.upgrade() {
        let fanout = hub.fanout.lock().unwrap_or_else(|err| err.into_inner());
        settle_locked(event, wrote, &fanout);
    }
}

// The caller holds the fanout lock.
fn settle_locked(event: &Logical, wrote: bool, fanout: &Fanout) {
    let release = {
        let mut track = event.track.lock().unwrap_or_else(|err| err.into_inner());
        if wrote {
            track.completes = track.completes.saturating_add(1);
        }
        track.holders = track.holders.saturating_sub(1);
        // An admitted catchup cursor is an obligation, not an event reference.
        // A healthy subscriber can finish while another drains bounded batches.
        let waiting = fanout.subs.iter().any(|sub| {
            !sub.dropped
                && sub.wants_key(&(event.session.clone(), event.epoch))
                && sub
                    .cursors
                    .get(&(event.session.clone(), event.epoch))
                    .is_some_and(|cursor| *cursor <= event.seq)
        });
        let release = track.holders == 0 && track.completes > 0 && !waiting;
        if release {
            track.retired = true;
        }
        release
    };
    if release && let Some(hub) = event.hub.upgrade() {
        for output in hub.live_sessions() {
            if output.session.session.as_str() == event.session {
                output.reap(event.index);
            }
        }
    }
}

fn retire_unheld(event: &Logical, fanout: &Fanout) {
    let unheld = event
        .track
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .holders
        == 0;
    if unheld {
        settle_locked(event, false, fanout);
    }
}

impl OutputSession {
    pub fn open(session: Arc<Session>, hub: Arc<Hub>) -> Arc<Self> {
        let output = Arc::new(Self {
            reliable: session.reliable().cloned(),
            session,
            hub: Arc::clone(&hub),
            inner: Mutex::new(OutInner {
                events: Vec::new(),
                staging: Vec::new(),
                eof: false,
            }),
            cv: Condvar::new(),
            order: Mutex::new(()),
            seq: AtomicU64::new(1),
            stop: AtomicBool::new(false),
            stall_noted: AtomicBool::new(false),
            fence: Mutex::new(None),
        });
        if output.session.mode() == fl_host_proto::SessionMode::LegacyPipe {
            hub.sessions
                .lock()
                .unwrap_or_else(|err| err.into_inner())
                .push(Arc::downgrade(&output));
        }
        output
    }

    pub(crate) fn session(&self) -> &Arc<Session> {
        &self.session
    }

    pub fn set_fence(&self, fence: Arc<dyn Fn() + Send + Sync>) {
        *self.fence.lock().unwrap_or_else(|err| err.into_inner()) = Some(fence);
    }

    pub fn emit_spawned(&self, child: HostedChild) {
        if let Some(journal) = &self.reliable {
            let result = journal.lifecycle(ControlEvent::Spawned {
                session: self.session.session.clone(),
                session_epoch: SessionEpoch(self.session.epoch),
                seq: 0,
                child,
            });
            debug_assert!(result.is_ok(), "pre-reserved finite lifecycle");
            return;
        }
        let session = self.session.session.clone();
        let epoch = self.session.epoch;
        self.publish_with(0, false, move |seq| {
            let event = ControlEvent::Spawned {
                session,
                session_epoch: SessionEpoch(epoch),
                seq,
                child,
            };
            let envelope = control_len(&event);
            (Body::Control(event), envelope)
        });
    }

    #[cfg(test)]
    fn emit_line(&self, raw: Vec<u8>) {
        self.emit_fragment(raw, true);
    }

    #[cfg(test)]
    fn emit_fragment(&self, raw: Vec<u8>, terminated: bool) {
        let raw_len = raw.len();
        let envelope = line_envelope(raw_len);
        let _order = self.lock_order();
        self.publish_locked(raw_len, false, terminated, move |_seq| {
            (Body::Line(raw), envelope)
        });
    }

    pub fn request_stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
        self.cv.notify_all();
    }

    pub(crate) fn stop_pumps(&self) {
        self.session.stop_pumps();
    }

    fn reap(&self, index: u64) {
        let mut inner = self.inner.lock().unwrap_or_else(|err| err.into_inner());
        let before = inner.events.len();
        inner.events.retain(|event| event.index != index);
        if inner.events.len() != before {
            self.cv.notify_all();
        }
    }

    fn lock_order(&self) -> std::sync::MutexGuard<'_, ()> {
        self.order.lock().unwrap_or_else(|err| err.into_inner())
    }

    /// `seq` and the fanout index are allocated under `order`, which is held through admit.
    fn publish_with(&self, raw_len: usize, exited: bool, build: impl FnOnce(u64) -> (Body, usize)) {
        let _order = self.lock_order();
        self.publish_locked(raw_len, exited, true, build);
    }

    fn publish_locked(
        &self,
        raw_len: usize,
        exited: bool,
        terminated: bool,
        build: impl FnOnce(u64) -> (Body, usize),
    ) {
        let seq = self.seq.fetch_add(1, Ordering::SeqCst);
        let index = self.hub.index.fetch_add(1, Ordering::SeqCst);
        let (body, envelope) = build(seq);
        let event = Arc::new(Logical {
            terminated,
            exposed: AtomicBool::new(false),
            index,
            seq,
            epoch: self.session.epoch,
            raw_len,
            envelope,
            session: self.session.session.as_str().to_owned(),
            exited,
            body,
            track: Mutex::new(Track {
                holders: 0,
                completes: 0,
                retired: false,
                delivered: HashSet::new(),
            }),
            hub: Arc::downgrade(&self.hub),
        });
        let key = (event.session.clone(), event.epoch);
        let (pending, next_seq) = {
            let mut inner = self.inner.lock().unwrap_or_else(|err| err.into_inner());
            inner.events.push(Arc::clone(&event));
            let pending = inner.events.clone();
            (pending, self.seq.load(Ordering::SeqCst))
        };
        // Drop `inner` before admit takes subscriber track locks. `order` still covers publish.
        self.hub.admit(&pending, &key, next_seq);
        self.cv.notify_all();
    }

    fn deliver_pending(&self) {
        // EOF can leave an admitted read parked behind the record cap. Retry
        // only one record here so the subscriber can return to socket writes.
        self.flush_one();
        let _order = self.lock_order();
        let key = (self.session.session.as_str().to_owned(), self.session.epoch);
        let (pending, next_seq) = {
            let inner = self.inner.lock().unwrap_or_else(|err| err.into_inner());
            (inner.events.clone(), self.seq.load(Ordering::SeqCst))
        };
        self.hub.admit(&pending, &key, next_seq);
        let fanout = self
            .hub
            .fanout
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        for event in &pending {
            event
                .track
                .lock()
                .unwrap_or_else(|err| err.into_inner())
                .delivered
                .retain(|id| fanout.subs.iter().any(|sub| !sub.dropped && sub.id == *id));
            // No holder is removed here; retry only deferred retirement.
            retire_unheld(event, &fanout);
        }
    }

    pub(crate) fn can_retire(&self) -> bool {
        if let Some(journal) = &self.reliable {
            return journal.is_eof()
                && journal.can_retire()
                && self.session.cleanup_report().is_some_and(|r| r.complete);
        }
        let inner = self.inner.lock().unwrap_or_else(|err| err.into_inner());
        inner.eof
            && inner.events.is_empty()
            && inner.staging.is_empty()
            && self
                .session
                .cleanup_report()
                .is_some_and(|report| report.complete)
    }

    fn charge(inner: &OutInner) -> Charge {
        let mut raw = inner.staging.len();
        let mut envelope: usize = 0;
        for event in &inner.events {
            raw = raw.saturating_add(event.raw_len);
            envelope = envelope.saturating_add(event.envelope);
        }
        Charge {
            events: inner.events.len(),
            charge: raw.saturating_add(envelope),
            raw,
        }
    }

    fn coalescible(inner: &OutInner) -> bool {
        inner.events.last().is_some_and(|tail| {
            !tail.terminated
                && !tail.exposed.load(Ordering::SeqCst)
                && Arc::strong_count(tail) == 1
                && matches!(&tail.body, Body::Line(raw) if raw.len() < MAX_STDOUT_LINE)
        })
    }

    fn note_stall(&self) {
        if self.stall_noted.swap(true, Ordering::SeqCst) {
            return;
        }
        let inner = self.inner.lock().unwrap_or_else(|err| err.into_inner());
        let charge = Self::charge(&inner);
        drop(inner);
        self.session.set_stalled(true);
        self.hub.note_status(charge);
    }
}

impl StdoutSink for OutputSession {
    fn reliable(&self) -> Option<&Arc<crate::reliable::ReliableJournal>> {
        self.reliable.as_ref()
    }
    fn room(&self) -> usize {
        if self.reliable.is_some() {
            return 0;
        }
        self.flush_out();
        self.space()
    }

    fn push(&self, bytes: &[u8]) -> bool {
        if self.reliable.is_some() {
            return false;
        }
        {
            let mut inner = self.inner.lock().unwrap_or_else(|err| err.into_inner());
            if !bytes.is_empty() {
                inner.staging.extend_from_slice(bytes);
            }
        }
        self.flush_out();
        if self.space() == 0 {
            self.note_stall();
        } else if !self.stop.load(Ordering::SeqCst) {
            self.session.set_stalled(false);
            self.stall_noted.store(false, Ordering::SeqCst);
        }
        true
    }

    fn eof(&self) {
        if let Some(journal) = &self.reliable {
            journal.eof();
        } else {
            self.inner.lock().unwrap_or_else(|e| e.into_inner()).eof = true;
            self.flush_out();
        }
        self.cv.notify_all();
    }

    fn wait(&self, timeout: Duration) {
        let inner = self.inner.lock().unwrap_or_else(|err| err.into_inner());
        let _ = self
            .cv
            .wait_timeout(inner, timeout)
            .unwrap_or_else(|err| err.into_inner());
    }

    fn on_leader(&self, exit: ChildExit) {
        if let Some(journal) = &self.reliable {
            let result = journal.lifecycle(ControlEvent::Exited {
                session: self.session.session.clone(),
                session_epoch: SessionEpoch(self.session.epoch),
                seq: 0,
                exit,
            });
            debug_assert!(result.is_ok(), "pre-reserved finite lifecycle");
            self.cv.notify_all();
            return;
        }
        let session = self.session.session.clone();
        let epoch = self.session.epoch;
        self.publish_with(0, true, move |seq| {
            let event = ControlEvent::Exited {
                session,
                session_epoch: SessionEpoch(epoch),
                seq,
                exit,
            };
            let envelope = control_len(&event);
            (Body::Control(event), envelope)
        });
    }

    fn on_cleanup(&self, complete: bool) {
        if complete && let Some(journal) = &self.reliable {
            let result = journal.lifecycle(ControlEvent::CleanupComplete {
                session: self.session.session.clone(),
                session_epoch: SessionEpoch(self.session.epoch),
                seq: 0,
            });
            debug_assert!(result.is_ok(), "pre-reserved finite lifecycle");
            self.fence.lock().unwrap_or_else(|e| e.into_inner()).take();
            self.cv.notify_all();
            return;
        }
        if !complete {
            return;
        }
        self.fence
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .take();
        let session = self.session.session.clone();
        let epoch = self.session.epoch;
        self.publish_with(0, false, move |seq| {
            let event = ControlEvent::CleanupComplete {
                session,
                session_epoch: SessionEpoch(epoch),
                seq,
            };
            let envelope = control_len(&event);
            (Body::Control(event), envelope)
        });
    }

    fn stopped(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }
}

impl OutputSession {
    fn flush_out(&self) {
        let key = (self.session.session.as_str().to_owned(), self.session.epoch);
        loop {
            // Let healthy writers drain a burst, without holding publication
            // order or allowing a slow subscriber to stall the producer forever.
            let started = std::time::Instant::now();
            while self.hub.all_backlogged(&key) && started.elapsed() < Duration::from_millis(20) {
                std::thread::sleep(Duration::from_millis(1));
            }
            if !self.flush_one() {
                return;
            }
        }
    }

    /// Parse, assign the sequence and admit one record atomically. Both the
    /// producer and catch-up workers can help drain parked bytes between writes.
    fn flush_one(&self) -> bool {
        let _order = self.lock_order();
        let mut inner = self.inner.lock().unwrap_or_else(|err| err.into_inner());
        if inner.staging.is_empty() {
            return false;
        }
        let coalesce = Self::coalescible(&inner);
        if inner.events.len() >= MAX_EVENTS && !coalesce {
            return false;
        }
        let room = if coalesce {
            MAX_STDOUT_LINE - inner.events.last().expect("tail").raw_len
        } else {
            MAX_STDOUT_LINE
        };
        let newline = inner.staging.iter().position(|b| *b == b'\n');
        let take = newline.unwrap_or(inner.staging.len()).min(room);
        let terminated = newline == Some(take);
        if coalesce {
            let new_len = inner.events.last().expect("tail").raw_len + take;
            let envelope = line_envelope(new_len);
            let old_envelope = inner.events.last().expect("tail").envelope;
            if Self::charge(&inner)
                .charge
                .saturating_add(envelope.saturating_sub(old_envelope))
                > MAX_STDOUT_BACKLOG
            {
                return false;
            }
            let OutInner {
                events, staging, ..
            } = &mut *inner;
            let tail = Arc::get_mut(events.last_mut().expect("tail")).expect("unexposed tail");
            let Body::Line(raw) = &mut tail.body else {
                unreachable!()
            };
            if raw.try_reserve_exact(take).is_err() {
                return false;
            }
            raw.extend_from_slice(&staging[..take]);
            staging.drain(..take + usize::from(terminated));
            tail.raw_len = raw.len();
            tail.envelope = envelope;
            tail.terminated = terminated;
            drop(inner);
            return true;
        }
        let envelope = line_envelope(take);
        if Self::charge(&inner).charge.saturating_add(envelope) > MAX_STDOUT_BACKLOG {
            return false;
        }
        let raw = inner.staging[..take].to_vec();
        inner.staging.drain(..take + usize::from(terminated));
        drop(inner);
        self.publish_locked(raw.len(), false, terminated, move |_| {
            (Body::Line(raw), envelope)
        });
        true
    }

    fn space(&self) -> usize {
        let inner = self.inner.lock().unwrap_or_else(|err| err.into_inner());
        if inner.eof || self.stop.load(Ordering::SeqCst) {
            return 0;
        }
        if !inner.staging.is_empty() {
            return 0;
        }
        if inner.events.len() >= MAX_EVENTS && !Self::coalescible(&inner) {
            return 0;
        }
        let charge = Self::charge(&inner);
        let budget = MAX_STDOUT_BACKLOG.saturating_sub(charge.charge);
        let budget = budget.saturating_sub(MAX_EVENT_ENVELOPE.min(budget));
        if budget == 0 {
            return 0;
        }
        budget.min(64 * 1024)
    }
}

impl Subscriber {
    pub fn abandon(self, hub: &Hub) {
        self.finish_drop(hub);
    }

    pub fn run(
        self,
        hub: Arc<Hub>,
        minor: u16,
        mut write: impl FnMut(Frame) -> Result<(), ()> + Send,
        mut closed: impl FnMut() -> bool + Send,
    ) {
        let hold = hold_dir();
        if let Some(dir) = hold.as_ref() {
            let _ = std::fs::write(dir.join(format!("subscribed-{}", self.id)), b"1");
        }
        let mut sent_any = false;
        loop {
            let next = self.next_event(&hub);
            let Some(event) = next else {
                if self.dropped(&hub) {
                    break;
                }
                if closed() {
                    self.finish_drop(&hub);
                    break;
                }
                let guard = self.lock.lock().unwrap_or_else(|err| err.into_inner());
                let _ = self
                    .wake
                    .wait_timeout(guard, Duration::from_millis(50))
                    .unwrap_or_else(|err| err.into_inner());
                continue;
            };
            if self.want_slow(&hub) && sent_any {
                let _ = self.write_slow(&hub, &mut write);
                self.finish_drop(&hub);
                break;
            }
            if let Some(dir) = hold.as_ref() {
                let phase = if event.exited {
                    "before-exited"
                } else {
                    "before-send"
                };
                // A targeted test barrier holds only this subscriber, so a healthy
                // peer can keep draining independently of kernel pipe capacity.
                let targeted = format!("{phase}-{}", self.id);
                let phase = if dir.join(format!("arm-{targeted}")).exists() {
                    targeted.as_str()
                } else {
                    phase
                };
                if !wait_hold(dir, phase, || {
                    self.dropped(&hub) || self.want_slow(&hub) || closed()
                }) {
                    if self.want_slow(&hub) && sent_any && !self.dropped(&hub) {
                        let _ = self.write_slow(&hub, &mut write);
                    }
                    self.finish_drop(&hub);
                    break;
                }
            }
            // A closed peer must not count as a finished write. The kernel can
            // accept a small frame after the reader is gone.
            if closed() {
                self.finish_drop(&hub);
                break;
            }
            if !event.terminated && minor < 2 {
                self.finish_drop(&hub);
                break;
            }
            let frames = frames_of(&event);
            let mut failed = frames.is_empty();
            for frame in frames {
                if write(frame).is_err() {
                    failed = true;
                    break;
                }
            }
            if failed {
                self.finish_drop(&hub);
                break;
            }
            self.pop_sent(&hub, &event);
            settle(&event, true);
            sent_any = true;
            hub.pull_preexisting();
        }
    }

    fn next_event(&self, hub: &Hub) -> Option<Arc<Logical>> {
        let fanout = hub.fanout.lock().unwrap_or_else(|err| err.into_inner());
        let sub = fanout.subs.iter().find(|sub| sub.id == self.id)?;
        if sub.dropped {
            return None;
        }
        let queued = sub.queue.front().map(Arc::clone);
        let terminal = sub
            .terminals
            .values()
            .min_by_key(|event| event.index)
            .map(Arc::clone);
        match (queued, terminal) {
            (Some(q), Some(t)) if t.index < q.index => Some(t),
            (Some(q), _) => Some(q),
            (None, Some(t)) => Some(t),
            (None, None) => None,
        }
    }

    fn pop_sent(&self, hub: &Hub, event: &Logical) {
        let mut fanout = hub.fanout.lock().unwrap_or_else(|err| err.into_inner());
        let Some(sub) = fanout.subs.iter_mut().find(|sub| sub.id == self.id) else {
            return;
        };
        if event.exited {
            if sub
                .terminals
                .get(&event.session)
                .is_some_and(|held| held.index == event.index)
            {
                sub.terminals.remove(&event.session);
            }
            return;
        }
        if let Some(pos) = sub
            .queue
            .iter()
            .position(|queued| queued.index == event.index)
        {
            let removed = sub.queue.remove(pos).expect("queued event");
            let bytes = removed.raw_len.saturating_add(removed.envelope);
            sub.queued_bytes = sub.queued_bytes.saturating_sub(bytes);
        }
    }

    fn dropped(&self, hub: &Hub) -> bool {
        let fanout = hub.fanout.lock().unwrap_or_else(|err| err.into_inner());
        fanout
            .subs
            .iter()
            .find(|sub| sub.id == self.id)
            .is_none_or(|sub| sub.dropped)
    }

    fn want_slow(&self, hub: &Hub) -> bool {
        let fanout = hub.fanout.lock().unwrap_or_else(|err| err.into_inner());
        fanout
            .subs
            .iter()
            .find(|sub| sub.id == self.id)
            .is_some_and(|sub| sub.slow)
    }

    fn write_slow(&self, hub: &Hub, write: &mut impl FnMut(Frame) -> Result<(), ()>) -> bool {
        let fanout = hub.fanout.lock().unwrap_or_else(|err| err.into_inner());
        let Some(sub) = fanout.subs.iter().find(|sub| sub.id == self.id) else {
            return false;
        };
        let Some(event) = sub.queue.front().or_else(|| sub.terminals.values().next()) else {
            return false;
        };
        let Ok(session) = SessionId::new(&event.session) else {
            return false;
        };
        let slow = ControlEvent::SlowSubscriber {
            session,
            session_epoch: SessionEpoch(event.epoch),
            seq: event.seq,
        };
        drop(fanout);
        let payload = serde_json::to_vec(&slow).unwrap_or_default();
        write(Frame {
            kind: FrameKind::Event,
            payload,
        })
        .is_ok()
    }

    fn finish_drop(&self, hub: &Hub) {
        let released = {
            let mut fanout = hub.fanout.lock().unwrap_or_else(|err| err.into_inner());
            take_sub(&mut fanout, self.id)
        };
        for event in released {
            settle(&event, false);
        }
        hub.pull_preexisting();
    }
}

fn frames_of(event: &Logical) -> Vec<Frame> {
    match &event.body {
        Body::Line(raw) => {
            let Ok(session) = SessionId::new(&event.session) else {
                return Vec::new();
            };
            fragment_line_with_termination(
                &session,
                SessionEpoch(event.epoch),
                event.seq,
                event.terminated,
                raw,
            )
            .unwrap_or_default()
        }
        Body::Control(control) => {
            let payload = serde_json::to_vec(control).unwrap_or_default();
            if payload.is_empty() || payload.len() > MAX_FRAME_PAYLOAD {
                return Vec::new();
            }
            vec![Frame {
                kind: FrameKind::Event,
                payload,
            }]
        }
    }
}

fn wait_hold(dir: &std::path::Path, phase: &str, mut dropped: impl FnMut() -> bool) -> bool {
    loop {
        if dropped() {
            return false;
        }
        match poll_barrier(dir, phase, None) {
            Barrier::Cleared => return !dropped(),
            Barrier::Pulsed | Barrier::Waiting | Barrier::Expired => {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }
}

impl Sub {
    fn wants_key(&self, key: &StreamKey) -> bool {
        self.filter.as_ref().is_none_or(|filter| filter == &key.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::SessionParts;

    fn fixture(epoch: u64, hub: &Arc<Hub>) -> Arc<OutputSession> {
        OutputSession::open(
            Arc::new(Session::new(SessionParts {
                session: SessionId::new("same").unwrap(),
                epoch,
                vendor_pid: 0,
                vendor_start: 0,
                supervisor_pid: 0,
                supervisor_start: 0,
                pgid: 0,
                control_inode: 0,
                control: None,
                stdin: None,
            })),
            Arc::clone(hub),
        )
    }

    #[test]
    fn old_subscription_refusal_preserves_bytes_and_freezes_exposed_tail() {
        let hub = Arc::new(Hub::new());
        let out = fixture(1, &hub);
        assert!(out.push(b"first"));
        let old = hub.subscribe(None, None).ok().unwrap();
        old.run(
            Arc::clone(&hub),
            1,
            |_| panic!("unsupported fragment sent"),
            || false,
        );
        assert_eq!(out.inner.lock().unwrap().events.len(), 1);
        assert!(out.push(b"second"));
        let inner = out.inner.lock().unwrap();
        assert_eq!(
            inner.events.len(),
            2,
            "previously exposed tail must stay frozen"
        );
        assert!(matches!(&inner.events[0].body, Body::Line(raw) if raw==b"first"));
        assert_eq!(inner.events[0].track.lock().unwrap().completes, 0);
    }

    #[test]
    fn eof_staging_resumes_after_backlog_retires() {
        let hub = Arc::new(Hub::new());
        let out = fixture(1, &hub);
        for _ in 0..MAX_EVENTS - 1 {
            assert!(out.push(b"x\n"));
        }
        assert!(out.push(b"a\nb"));
        out.eof();
        let sub = hub.subscribe(None, None).ok().unwrap();
        let mut got = Vec::new();
        while let Some(event) = sub.next_event(&hub) {
            if let Body::Line(raw) = &event.body {
                got.extend_from_slice(raw);
                if event.terminated {
                    got.push(b'\n');
                }
            }
            sub.pop_sent(&hub, &event);
            settle(&event, true);
            hub.pull_preexisting();
        }
        assert_eq!(got.len(), (MAX_EVENTS - 1) * 2 + 3);
        assert!(got.ends_with(b"a\nb"));
        sub.abandon(&hub);
    }

    #[test]
    fn tiny_unattended_reads_coalesce_and_reconstruct_exact_bytes() {
        let hub = Arc::new(Hub::new());
        let out = fixture(1, &hub);
        let mut expected = Vec::new();
        for i in 0..100_000 {
            let raw: &[u8] = if i % 1000 == 999 { b"x\n" } else { b"x" };
            assert!(out.room() >= raw.len());
            assert!(out.push(raw));
            expected.extend_from_slice(raw);
        }
        assert_eq!(out.inner.lock().unwrap().events.len(), 100);
        let sub = hub.subscribe(None, None).ok().unwrap();
        let mut got = Vec::new();
        while let Some(event) = sub.next_event(&hub) {
            if let Body::Line(raw) = &event.body {
                got.extend_from_slice(raw);
                if event.terminated {
                    got.push(b'\n');
                }
            }
            sub.pop_sent(&hub, &event);
            settle(&event, true);
            hub.pull_preexisting();
        }
        assert_eq!(got, expected);
        sub.abandon(&hub);
    }
    #[test]
    fn bounded_fragments_preserve_binary_lf_boundary_and_eof() {
        let hub = Arc::new(Hub::new());
        let out = fixture(1, &hub);
        let mut expected = vec![b'z'; 2 * MAX_STDOUT_LINE];
        expected.extend_from_slice(b"\n\r\x1b[0m\0\xfftail");
        for raw in expected.chunks(4096) {
            assert!(out.push(raw));
        }
        out.eof();
        let inner = out.inner.lock().unwrap();
        let mut got = Vec::new();
        for event in &inner.events {
            if let Body::Line(raw) = &event.body {
                assert!(raw.len() <= MAX_STDOUT_LINE);
                got.extend_from_slice(raw);
                if event.terminated {
                    got.push(b'\n');
                }
            } else {
                panic!("no protocol failure");
            }
        }
        assert_eq!(got, expected);
    }

    #[test]
    fn cursor_disconnect_releases_completed_shared_backlog() {
        let hub = Arc::new(Hub::new());
        let output = fixture(1, &hub);
        for _ in 0..100 {
            output.emit_line(vec![1; 64 * 1024]);
        }
        let slow = hub.subscribe(None, None).ok().unwrap();
        let fast = hub.subscribe(None, None).ok().unwrap();
        for _ in 0..100 {
            let event = fast.next_event(&hub).expect("catchup");
            fast.pop_sent(&hub, &event);
            settle(&event, true);
            hub.pull_preexisting();
        }
        assert!(OutputSession::charge(&output.inner.lock().unwrap()).charge > SUB_BYTES);
        slow.abandon(&hub);
        assert_eq!(
            OutputSession::charge(&output.inner.lock().unwrap()).charge,
            0
        );
        fast.abandon(&hub);
    }

    #[test]
    fn occupied_same_name_exit_slot_drops_slow_subscriber_without_overwrite() {
        let hub = Arc::new(Hub::new());
        let first = fixture(1, &hub);
        let second = fixture(2, &hub);
        let sub = hub.subscribe(None, None).ok().unwrap();
        first.on_leader(ChildExit {
            code: Some(1),
            signal: None,
        });
        second.on_leader(ChildExit {
            code: Some(2),
            signal: None,
        });
        assert_eq!(hub.fanout.lock().unwrap().subs[0].terminals.len(), 1);
        assert!(sub.want_slow(&hub));
        assert_eq!(sub.next_event(&hub).unwrap().epoch, 1);
        sub.abandon(&hub);
        let next = hub.subscribe(None, None).ok().unwrap();
        assert!(next.want_slow(&hub));
        assert_eq!(next.next_event(&hub).unwrap().epoch, 1);
        next.abandon(&hub);
    }

    #[test]
    fn historical_occupied_exit_slot_marks_slow_and_teardown_preserves_unread_output() {
        let hub = Arc::new(Hub::new());
        let first = fixture(1, &hub);
        let second = fixture(2, &hub);
        first.emit_line(b"first epoch".to_vec());
        first.on_leader(ChildExit {
            code: Some(1),
            signal: None,
        });
        second.emit_line(b"second epoch".to_vec());
        second.on_leader(ChildExit {
            code: Some(2),
            signal: None,
        });

        let sub = hub.subscribe(None, None).ok().unwrap();
        assert!(sub.want_slow(&hub));
        {
            let fanout = hub.fanout.lock().unwrap();
            let state = &fanout.subs[0];
            assert_eq!(state.queue.len(), 2);
            assert_eq!(state.terminals.len(), 1);
            let terminal = state.terminals.get("same").unwrap();
            assert_eq!(terminal.epoch, 1);
            assert!(matches!(
                &terminal.body,
                Body::Control(ControlEvent::Exited { exit, .. }) if exit.code == Some(1)
            ));
            let pending = second.inner.lock().unwrap();
            let refused = pending.events.iter().find(|event| event.exited).unwrap();
            let track = refused.track.lock().unwrap();
            assert_eq!(track.holders, 0);
            assert!(!track.delivered.contains(&sub.id));
        }

        sub.abandon(&hub);
        {
            let fanout = hub.fanout.lock().unwrap();
            let state = &fanout.subs[0];
            assert!(state.dropped);
            assert!(state.queue.is_empty());
            assert_eq!(state.queued_bytes, 0);
            assert!(state.terminals.is_empty());
            assert!(state.cursors.is_empty());
        }
        for (output, expected) in [
            (&first, b"first epoch".as_slice()),
            (&second, b"second epoch".as_slice()),
        ] {
            let pending = output.inner.lock().unwrap();
            assert_eq!(pending.events.len(), 2);
            assert!(matches!(&pending.events[0].body, Body::Line(raw) if raw == expected));
            for event in &pending.events {
                let track = event.track.lock().unwrap();
                assert_eq!(track.holders, 0);
                assert_eq!(track.completes, 0);
                assert!(!track.retired);
            }
        }
    }

    #[test]
    fn small_event_catchup_stays_within_64_references() {
        let hub = Arc::new(Hub::new());
        let output = fixture(1, &hub);
        for _ in 0..128 {
            output.emit_line(Vec::new());
        }
        let sub = hub.subscribe(None, None).ok().unwrap();
        assert_eq!(hub.fanout.lock().unwrap().subs[0].queue.len(), SUB_QUEUE);
        for expected in 1..=128 {
            let event = sub.next_event(&hub).expect("small event catchup");
            assert_eq!(event.seq, expected);
            sub.pop_sent(&hub, &event);
            settle(&event, true);
            hub.pull_preexisting();
            let fanout = hub.fanout.lock().unwrap();
            assert!(fanout.subs[0].queue.len() <= SUB_QUEUE);
        }
        assert_eq!(
            OutputSession::charge(&output.inner.lock().unwrap()).charge,
            0
        );
        sub.abandon(&hub);
    }

    #[test]
    fn catchup_references_stay_bounded_and_drain_in_order() {
        let hub = Arc::new(Hub::new());
        let session = Arc::new(Session::new(SessionParts {
            session: SessionId::new("parked").unwrap(),
            epoch: 1,
            vendor_pid: 0,
            vendor_start: 0,
            supervisor_pid: 0,
            supervisor_start: 0,
            pgid: 0,
            control_inode: 0,
            control: None,
            stdin: None,
        }));
        let output = OutputSession::open(session, Arc::clone(&hub));
        for _ in 0..100 {
            output.emit_line(vec![1; 64 * 1024]);
        }
        let slow = hub.subscribe(None, None).ok().unwrap();
        let fast = hub.subscribe(None, None).ok().unwrap();
        {
            let fanout = hub.fanout.lock().unwrap();
            for sub in &fanout.subs {
                assert!(sub.queue.len() <= SUB_QUEUE);
                assert!(sub.queued_bytes <= SUB_BYTES);
            }
        }
        for expected in 1..=100 {
            let event = fast.next_event(&hub).expect("fast catchup gap");
            assert_eq!(event.seq, expected);
            fast.pop_sent(&hub, &event);
            settle(&event, true);
            hub.pull_preexisting();
        }
        let newcomer = hub.subscribe(None, None).ok().unwrap();
        assert!(
            newcomer.next_event(&hub).is_none(),
            "completed output was replayed to a new subscriber"
        );
        newcomer.abandon(&hub);
        // A completed fast write must not erase a slow subscriber's catchup.
        for expected in 1..=100 {
            let event = slow.next_event(&hub).expect("slow catchup gap");
            assert_eq!(event.seq, expected);
            slow.pop_sent(&hub, &event);
            settle(&event, true);
            hub.pull_preexisting();
        }
        assert_eq!(
            OutputSession::charge(&output.inner.lock().unwrap()).charge,
            0
        );
        slow.abandon(&hub);
        fast.abandon(&hub);
    }
}
