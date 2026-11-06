//! Shared stdout backlog and bounded event subscribers.
//!
//! Storage is charged once. Subscribers hold references. A socket write is not
//! application consumption, and a dropped subscriber does not discard bytes that
//! nobody has fully written.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use fl_host_proto::{
    ChildExit, ControlEvent, Frame, FrameKind, HostedChild, LINE_HEADER_LEN, MAX_EVENT_ENVELOPE,
    MAX_FRAME_PAYLOAD, MAX_STDOUT_BACKLOG, MAX_STDOUT_LINE, MAX_SUBSCRIBERS, SessionEpoch,
    SessionId, ShutdownSwitch, fragment_line,
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

enum Body {
    Line(Vec<u8>),
    Control(ControlEvent),
}

struct Track {
    holders: usize,
    completes: usize,
    delivered: HashSet<u64>,
}

struct Logical {
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

pub struct Hub {
    fanout: Mutex<Fanout>,
    sessions: Mutex<Vec<std::sync::Weak<OutputSession>>>,
    index: AtomicU64,
    status: Option<PathBuf>,
    status_lock: Mutex<()>,
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
}

pub struct OutputSession {
    session: Arc<Session>,
    hub: Arc<Hub>,
    inner: Mutex<OutInner>,
    cv: Condvar,
    seq: AtomicU64,
    stop: AtomicBool,
    stall_noted: AtomicBool,
    fence: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

struct OutInner {
    events: Vec<Arc<Logical>>,
    partial: Vec<u8>,
    staging: Vec<u8>,
    failed: bool,
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
        }
    }

    pub fn subscribe(
        &self,
        filter: Option<SessionId>,
        kill: Option<Arc<ShutdownSwitch>>,
    ) -> Result<Subscriber, (Option<Arc<ShutdownSwitch>>, &'static str)> {
        let mut fanout = self.fanout.lock().unwrap_or_else(|err| err.into_inner());
        let alive = fanout.subs.iter().filter(|sub| !sub.dropped).count();
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
        });
        drop(fanout);
        self.pull_preexisting();
        Ok(Subscriber { id, wake, lock })
    }

    fn pull_preexisting(&self) {
        let sessions = self.live_sessions();
        let mut pending = Vec::new();
        for session in &sessions {
            let inner = session.inner.lock().unwrap_or_else(|err| err.into_inner());
            for event in &inner.events {
                let track = event.track.lock().unwrap_or_else(|err| err.into_inner());
                if track.holders == 0 && track.completes == 0 {
                    pending.push(Arc::clone(event));
                }
            }
        }
        pending.sort_by_key(|event| event.index);
        let mut fanout = self.fanout.lock().unwrap_or_else(|err| err.into_inner());
        for event in pending {
            for sub in fanout.subs.iter_mut().filter(|sub| !sub.dropped) {
                if event.index >= sub.subscribed_at || !sub.wants(&event) {
                    continue;
                }
                let _ = assign(sub, &event, true);
            }
        }
        for sub in &fanout.subs {
            sub.wake.notify_all();
        }
    }

    fn live_sessions(&self) -> Vec<Arc<OutputSession>> {
        let mut slots = self.sessions.lock().unwrap_or_else(|err| err.into_inner());
        slots.retain(|slot| slot.strong_count() > 0);
        slots.iter().filter_map(|slot| slot.upgrade()).collect()
    }

    fn publish(&self, event: Arc<Logical>) {
        let mut fanout = self.fanout.lock().unwrap_or_else(|err| err.into_inner());
        for sub in fanout.subs.iter_mut() {
            if sub.dropped || !sub.wants(&event) {
                continue;
            }
            // Leave the queue intact so the writer can emit SlowSubscriber at a
            // message boundary, then drop itself. Overflow never overwrites a slot.
            if assign(sub, &event, false) == Assign::Overflow {
                sub.slow = true;
            }
        }
        for sub in &fanout.subs {
            sub.wake.notify_all();
        }
    }

    pub fn all_backlogged(&self) -> bool {
        let fanout = self.fanout.lock().unwrap_or_else(|err| err.into_inner());
        let live: Vec<_> = fanout.subs.iter().filter(|sub| !sub.dropped).collect();
        !live.is_empty() && live.iter().all(|sub| sub.queue.len() >= 32)
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
    id: u64,
    wake: Arc<Condvar>,
    lock: Arc<Mutex<()>>,
}

fn assign(sub: &mut Sub, event: &Arc<Logical>, preexisting: bool) -> Assign {
    if sub.dropped {
        return Assign::Skip;
    }
    let mut track = event.track.lock().unwrap_or_else(|err| err.into_inner());
    if !track.delivered.insert(sub.id) {
        return Assign::Skip;
    }
    let bytes = event.raw_len.saturating_add(event.envelope);
    if event.exited {
        if sub.terminals.contains_key(&event.session) || sub.terminals.len() >= TERMINAL_SLOTS {
            track.delivered.remove(&sub.id);
            return if preexisting {
                Assign::Skip
            } else {
                Assign::Overflow
            };
        }
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

fn take_sub(fanout: &mut Fanout, id: u64) -> Vec<Arc<Logical>> {
    let Some(sub) = fanout.subs.iter_mut().find(|sub| sub.id == id) else {
        return Vec::new();
    };
    if sub.dropped {
        return Vec::new();
    }
    sub.dropped = true;
    sub.slow = true;
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
    let release = {
        let mut track = event.track.lock().unwrap_or_else(|err| err.into_inner());
        if wrote {
            track.completes = track.completes.saturating_add(1);
        }
        track.holders = track.holders.saturating_sub(1);
        track.holders == 0 && track.completes > 0
    };
    if release && let Some(hub) = event.hub.upgrade() {
        for output in hub.live_sessions() {
            if output.session.session.as_str() == event.session {
                output.reap(event.index);
            }
        }
    }
}

impl OutputSession {
    pub fn open(session: Arc<Session>, hub: Arc<Hub>) -> Arc<Self> {
        let output = Arc::new(Self {
            session,
            hub: Arc::clone(&hub),
            inner: Mutex::new(OutInner {
                events: Vec::new(),
                partial: Vec::new(),
                staging: Vec::new(),
                failed: false,
                eof: false,
            }),
            cv: Condvar::new(),
            seq: AtomicU64::new(1),
            stop: AtomicBool::new(false),
            stall_noted: AtomicBool::new(false),
            fence: Mutex::new(None),
        });
        hub.sessions
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .push(Arc::downgrade(&output));
        output
    }

    pub fn set_fence(&self, fence: Arc<dyn Fn() + Send + Sync>) {
        *self.fence.lock().unwrap_or_else(|err| err.into_inner()) = Some(fence);
    }

    pub fn emit_spawned(&self, child: HostedChild) {
        let seq = self.seq.fetch_add(1, Ordering::SeqCst);
        self.emit_control(
            ControlEvent::Spawned {
                session: self.session.session.clone(),
                session_epoch: SessionEpoch(self.session.epoch),
                seq,
                child,
            },
            false,
            seq,
        );
    }

    fn emit_control(&self, event: ControlEvent, exited: bool, seq: u64) {
        let bytes = serde_json::to_vec(&event).unwrap_or_default();
        let envelope = bytes.len().clamp(1, MAX_EVENT_ENVELOPE);
        self.push_event(Body::Control(event), 0, envelope, exited, seq);
    }

    fn emit_line(&self, raw: Vec<u8>) {
        let started = Instant::now();
        while self.hub.all_backlogged() && started.elapsed() < Duration::from_millis(20) {
            std::thread::sleep(Duration::from_millis(1));
        }
        let envelope = line_envelope(raw.len());
        let raw_len = raw.len();
        let seq = self.seq.fetch_add(1, Ordering::SeqCst);
        self.push_event(Body::Line(raw), raw_len, envelope, false, seq);
    }

    pub fn request_stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
        self.cv.notify_all();
    }

    fn reap(&self, index: u64) {
        let mut inner = self.inner.lock().unwrap_or_else(|err| err.into_inner());
        let before = inner.events.len();
        inner.events.retain(|event| event.index != index);
        if inner.events.len() != before {
            self.cv.notify_all();
        }
    }

    fn push_event(&self, body: Body, raw_len: usize, envelope: usize, exited: bool, seq: u64) {
        let index = self.hub.index.fetch_add(1, Ordering::SeqCst);
        let event = Arc::new(Logical {
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
                delivered: HashSet::new(),
            }),
            hub: Arc::downgrade(&self.hub),
        });
        {
            let mut inner = self.inner.lock().unwrap_or_else(|err| err.into_inner());
            inner.events.push(Arc::clone(&event));
        }
        self.hub.publish(event);
        self.cv.notify_all();
    }

    fn charge(inner: &OutInner) -> Charge {
        let mut raw = inner.partial.len().saturating_add(inner.staging.len());
        let mut envelope = if inner.partial.is_empty() {
            0
        } else {
            line_envelope(inner.partial.len())
        };
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

    fn fail_overflow(&self) {
        let first = {
            let mut inner = self.inner.lock().unwrap_or_else(|err| err.into_inner());
            if inner.failed {
                false
            } else {
                inner.failed = true;
                inner.partial.clear();
                inner.staging.clear();
                true
            }
        };
        if !first {
            return;
        }
        let seq = self.seq.fetch_add(1, Ordering::SeqCst);
        self.emit_control(
            ControlEvent::ProtocolFailure {
                session: self.session.session.clone(),
                session_epoch: SessionEpoch(self.session.epoch),
                seq,
                reason: "stdout line exceeds 1 MiB".into(),
            },
            false,
            seq,
        );
        if let Some(fence) = self
            .fence
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .clone()
        {
            fence();
        }
        let _ = self.session.request_cleanup();
        self.cv.notify_all();
    }

    fn drain(inner: &mut OutInner) -> Parsed {
        if inner.failed || inner.staging.is_empty() {
            return Parsed::Need;
        }
        if inner.partial.is_empty() && inner.events.len() >= MAX_EVENTS {
            return Parsed::Need;
        }
        if inner.partial.len() > MAX_STDOUT_LINE {
            return Parsed::Fail;
        }
        if inner.partial.len() == MAX_STDOUT_LINE {
            if inner.staging[0] == b'\n' {
                inner.staging.remove(0);
                return Parsed::Line(std::mem::take(&mut inner.partial));
            }
            return Parsed::Fail;
        }
        if let Some(pos) = inner.staging.iter().position(|byte| *byte == b'\n') {
            let mut line = std::mem::take(&mut inner.partial);
            line.extend_from_slice(&inner.staging[..pos]);
            inner.staging.drain(..=pos);
            if line.len() > MAX_STDOUT_LINE {
                return Parsed::Fail;
            }
            return Parsed::Line(line);
        }
        let room = MAX_STDOUT_LINE.saturating_sub(inner.partial.len());
        if room == 0 {
            return Parsed::Fail;
        }
        let take = room.min(inner.staging.len());
        let extra: Vec<u8> = inner.staging.drain(..take).collect();
        inner.partial.extend(extra);
        if inner.partial.len() > MAX_STDOUT_LINE {
            return Parsed::Fail;
        }
        Parsed::Need
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

enum Parsed {
    Line(Vec<u8>),
    Fail,
    Need,
}

impl StdoutSink for OutputSession {
    fn room(&self) -> usize {
        if self.flush_out() {
            self.fail_overflow();
            return 0;
        }
        self.space()
    }

    fn push(&self, bytes: &[u8]) -> bool {
        {
            let mut inner = self.inner.lock().unwrap_or_else(|err| err.into_inner());
            if inner.failed {
                return false;
            }
            if !bytes.is_empty() {
                inner.staging.extend_from_slice(bytes);
            }
        }
        let overflow = self.flush_out();
        if overflow {
            self.fail_overflow();
            return false;
        }
        if self.space() == 0 {
            self.note_stall();
        } else if !self.stop.load(Ordering::SeqCst) {
            self.session.set_stalled(false);
            self.stall_noted.store(false, Ordering::SeqCst);
        }
        true
    }

    fn eof(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(|err| err.into_inner());
        inner.eof = true;
        inner.partial.clear();
        inner.staging.clear();
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
        let seq = self.seq.fetch_add(1, Ordering::SeqCst);
        self.emit_control(
            ControlEvent::Exited {
                session: self.session.session.clone(),
                session_epoch: SessionEpoch(self.session.epoch),
                seq,
                exit,
            },
            true,
            seq,
        );
    }

    fn on_cleanup(&self, complete: bool) {
        if !complete {
            return;
        }
        let seq = self.seq.fetch_add(1, Ordering::SeqCst);
        self.emit_control(
            ControlEvent::CleanupComplete {
                session: self.session.session.clone(),
                session_epoch: SessionEpoch(self.session.epoch),
                seq,
            },
            false,
            seq,
        );
    }

    fn stopped(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }
}

impl OutputSession {
    fn flush_out(&self) -> bool {
        loop {
            let next = {
                let mut inner = self.inner.lock().unwrap_or_else(|err| err.into_inner());
                if inner.failed {
                    return true;
                }
                match Self::drain(&mut inner) {
                    Parsed::Line(line) => Some(line),
                    Parsed::Fail => return true,
                    Parsed::Need => None,
                }
            };
            let Some(line) = next else {
                return false;
            };
            self.emit_line(line);
        }
    }

    fn space(&self) -> usize {
        let inner = self.inner.lock().unwrap_or_else(|err| err.into_inner());
        if inner.failed || inner.eof || self.stop.load(Ordering::SeqCst) {
            return 0;
        }
        if !inner.staging.is_empty() {
            return 0;
        }
        if inner.events.len() >= MAX_EVENTS && inner.partial.is_empty() {
            return 0;
        }
        let charge = Self::charge(&inner);
        let budget = MAX_STDOUT_BACKLOG.saturating_sub(charge.charge);
        let budget = budget.saturating_sub(MAX_EVENT_ENVELOPE.min(budget));
        if budget == 0 {
            return 0;
        }
        let line_left = (MAX_STDOUT_LINE + 1).saturating_sub(inner.partial.len());
        budget.min(line_left).min(64 * 1024)
    }
}

impl Subscriber {
    pub fn abandon(self, hub: &Hub) {
        self.finish_drop(hub);
    }

    pub fn run(
        self,
        hub: Arc<Hub>,
        mut write: impl FnMut(Frame) -> Result<(), ()> + Send,
        mut closed: impl FnMut() -> bool + Send,
    ) {
        let hold = hold_dir();
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
            fragment_line(&session, SessionEpoch(event.epoch), event.seq, raw).unwrap_or_default()
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
    fn wants(&self, event: &Logical) -> bool {
        self.filter
            .as_ref()
            .is_none_or(|filter| filter == &event.session)
    }
}
