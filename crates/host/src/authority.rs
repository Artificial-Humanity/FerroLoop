//! Epoch authority outlives the latest stdin writer. All registry collections are finite.
use crate::reliable::{self, ReliableJournal, ReplayCursor};
use crate::slice_budget::{ChargeKind, Reservation, SliceBudget, reserve};
use crate::{
    HostInner,
    input::WriterHandle,
    output::{Hub, SubscriberSlot},
    session::Session,
};
use fl_host_proto::{
    AuthToken, ConnId, ControllerLease, Cursor, ExtensionRefusal as Refusal, ExtensionResponse,
    HostInstance, StreamId, Ticket,
};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

const MAX_CONNECTIONS: usize = 128;
const TICKET_LIFETIME: Duration = Duration::from_secs(5);
const ARC_OVERHEAD: usize = 2 * std::mem::size_of::<usize>();

pub(crate) struct PreparedEpoch {
    pub(crate) journal: Arc<ReliableJournal>,
    metadata: Reservation,
}
struct Epoch {
    journal: Arc<ReliableJournal>,
    session: Weak<Session>,
    writer: Weak<WriterHandle>,
    generation: u64,
    claiming: Option<u64>,
    owner: Option<Owner>,
    active: Option<Arc<StreamState>>,
    final_write: Option<Arc<FinalWriteState>>,
    _metadata: Reservation,
}
#[derive(Clone, Copy)]
struct Owner {
    conn: ConnId,
    lease: ControllerLease,
    writer_lease: Option<ControllerLease>,
    ceiling: u64,
}
struct Pending {
    token: Ticket,
    rpc: ConnId,
    stream: StreamId,
    lease: Option<ControllerLease>,
    expires: Instant,
    cursor: ReplayCursor,
    slot: SubscriberSlot,
    metadata: Reservation,
}
pub(crate) struct StreamState {
    cancelled: AtomicBool,
}
struct State {
    rpc_connections: Vec<ConnId>,
    epochs: Vec<Epoch>,
    pending: Vec<Pending>,
    next: u64,
}
pub(crate) struct AuthorityRegistry {
    host: HostInstance,
    context: Mutex<Weak<HostInner>>,
    hub: Arc<Hub>,
    state: Mutex<State>,
    budget: Arc<SliceBudget>,
    _metadata: Reservation,
}
pub(crate) struct ControllerGrant {
    pub(crate) lease: ControllerLease,
    pub(crate) acked: u64,
    pub(crate) latest: u64,
}
pub(crate) struct TicketGrant {
    pub(crate) ticket: Ticket,
    pub(crate) acked: u64,
    pub(crate) latest: u64,
    pub(crate) watermark: u64,
}
pub(crate) struct StreamGuard {
    registry: Weak<AuthorityRegistry>,
    pub(crate) state: Arc<StreamState>,
    stream: StreamId,
    rpc: ConnId,
    lease: Option<ControllerLease>,
    slot: Option<SubscriberSlot>,
    metadata: Option<Reservation>,
}
impl Drop for StreamGuard {
    fn drop(&mut self) {
        drop(self.slot.take());
        drop(self.metadata.take());
        if let Some(reg) = self.registry.upgrade() {
            reg.finish(&self.stream, &self.state);
        }
    }
}
impl StreamGuard {
    pub(crate) fn rpc(&self) -> ConnId {
        self.rpc
    }
}

impl StreamState {
    pub(crate) fn cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}
// One final write per active stream. These inline slots bound retained task
// wakers to the same 64 operations admitted on the sole owning RPC connection.
const ACK_WAITERS: usize = 64;
struct Completion {
    terminal: bool,
    waiters: [Option<Waker>; ACK_WAITERS],
}
struct FinalWriteState {
    seq: u64,
    deadline: Instant,
    settled: Mutex<Completion>,
    _metadata: Reservation,
}
impl FinalWriteState {
    fn settle(&self) {
        let mut state = self.settled.lock().unwrap_or_else(|e| e.into_inner());
        if state.terminal {
            return;
        }
        state.terminal = true;
        for waiter in &mut state.waiters {
            if let Some(waker) = waiter.take() {
                waker.wake();
            }
        }
    }
}
fn final_write_storage_bound() -> usize {
    // Allocation + Arc header + retained producer guard and registry Arc handle.
    // Waiter-held Arc handles are included in each charged Timeout<AckWait>.
    std::mem::size_of::<FinalWriteState>()
        + ARC_OVERHEAD
        + std::mem::size_of::<FinalWriteGuard<'_>>()
        + std::mem::size_of::<Arc<FinalWriteState>>()
}
pub(crate) struct FinalWriteGuard<'a> {
    registry: &'a AuthorityRegistry,
    stream: &'a StreamGuard,
    completion: Arc<FinalWriteState>,
}
impl FinalWriteGuard<'_> {
    pub(crate) fn complete(self) {
        let mut state = self
            .registry
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Ok(epoch) = self.registry.epoch(&mut state, &self.stream.stream)
            && epoch
                .final_write
                .as_ref()
                .is_some_and(|pending| Arc::ptr_eq(pending, &self.completion))
        {
            if !self.stream.state.cancelled()
                && Instant::now() < self.completion.deadline
                && epoch
                    .active
                    .as_ref()
                    .is_some_and(|active| Arc::ptr_eq(active, &self.stream.state))
                && let Some(owner) = epoch.owner.as_mut()
                && self.stream.lease == Some(owner.lease)
                && self.stream.rpc == owner.conn
            {
                owner.ceiling = owner.ceiling.max(self.completion.seq);
            }
            // Credit and terminal state change under the same authority lock.
            epoch.final_write.take();
            self.completion.settle();
        }
    }
}
impl Drop for FinalWriteGuard<'_> {
    fn drop(&mut self) {
        let mut state = self
            .registry
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Ok(epoch) = self.registry.epoch(&mut state, &self.stream.stream)
            && epoch
                .final_write
                .as_ref()
                .is_some_and(|pending| Arc::ptr_eq(pending, &self.completion))
        {
            epoch.final_write.take();
        }
        // Also handles task cancellation after successful bytes but before publish.
        self.completion.settle();
    }
}
fn cancel_stream(epoch: &mut Epoch) {
    if let Some(active) = epoch.active.as_ref() {
        active.cancelled.store(true, Ordering::SeqCst);
    }
    if let Some(pending) = epoch.final_write.take() {
        pending.settle();
    }
}
struct AckWait {
    completion: Arc<FinalWriteState>,
    slot: Option<usize>,
    _metadata: Reservation,
}
impl Future for AckWait {
    type Output = ();
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let this = self.get_mut();
        let mut state = this
            .completion
            .settled
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if state.terminal {
            return Poll::Ready(());
        }
        let slot = match this.slot {
            Some(index) => index,
            None => {
                // The sole owning RPC has at most 64 admitted operations.
                // Defensive overflow settles this wait without granting credit.
                let Some(index) = state.waiters.iter().position(Option::is_none) else {
                    return Poll::Ready(());
                };
                this.slot = Some(index);
                index
            }
        };
        if !state.waiters[slot]
            .as_ref()
            .is_some_and(|waker| waker.will_wake(cx.waker()))
        {
            state.waiters[slot] = Some(cx.waker().clone());
        }
        if let Some(dir) = crate::input::hold_dir() {
            let _ = std::fs::write(dir.join("at-reliable-ack-wait"), b"1");
        }
        Poll::Pending
    }
}
impl Drop for AckWait {
    fn drop(&mut self) {
        if let Some(index) = self.slot {
            self.completion
                .settled
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .waiters[index] = None;
        }
    }
}
impl AuthorityRegistry {
    pub(crate) fn new(budget: Arc<SliceBudget>, hub: Arc<Hub>) -> Result<Arc<Self>, Refusal> {
        let metadata = reserve(
            &budget,
            ChargeKind::Metadata,
            std::mem::size_of::<Self>()
                + MAX_CONNECTIONS * std::mem::size_of::<ConnId>()
                + 64 * std::mem::size_of::<Epoch>()
                + 16 * std::mem::size_of::<Pending>(),
        )?;
        let random = AuthToken::generate().map_err(|_| Refusal::Io)?;
        let mut host = [0; 16];
        host.copy_from_slice(&random.as_bytes()[..16]);
        Ok(Arc::new(Self {
            host: HostInstance::new(host),
            context: Mutex::new(Weak::new()),
            hub,
            state: Mutex::new(State {
                rpc_connections: Vec::with_capacity(MAX_CONNECTIONS),
                epochs: Vec::with_capacity(64),
                pending: Vec::with_capacity(16),
                next: 1,
            }),
            budget,
            _metadata: metadata,
        }))
    }
    pub(crate) fn bind(&self, host: &Arc<HostInner>) {
        *self.context.lock().unwrap_or_else(|e| e.into_inner()) = Arc::downgrade(host);
    }
    pub(crate) fn connection_live(&self, conn: ConnId) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .rpc_connections
            .contains(&conn)
    }
    pub(crate) fn instance(&self) -> &HostInstance {
        &self.host
    }
    pub(crate) fn connect(&self, conn: ConnId) -> Result<(), Refusal> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.rpc_connections.len() == MAX_CONNECTIONS {
            return Err(Refusal::ResourceLimit);
        }
        state.rpc_connections.push(conn);
        Ok(())
    }
    pub(crate) fn prepare_epoch(
        &self,
        journal: Arc<ReliableJournal>,
    ) -> Result<PreparedEpoch, Refusal> {
        let metadata = reserve(
            &self.budget,
            ChargeKind::Metadata,
            std::mem::size_of::<Epoch>() + 64,
        )?;
        Ok(PreparedEpoch { journal, metadata })
    }
    pub(crate) fn register(
        &self,
        prepared: PreparedEpoch,
        session: &Arc<Session>,
        writer: &Arc<WriterHandle>,
    ) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        assert!(
            state.epochs.len() < 64,
            "spawn admission holds retained epoch capacity"
        );
        state.epochs.push(Epoch {
            journal: prepared.journal,
            session: Arc::downgrade(session),
            writer: Arc::downgrade(writer),
            generation: 0,
            claiming: None,
            owner: None,
            active: None,
            final_write: None,
            _metadata: prepared.metadata,
        });
    }
    fn epoch<'a>(&self, state: &'a mut State, stream: &StreamId) -> Result<&'a mut Epoch, Refusal> {
        if stream.host != self.host {
            return Err(Refusal::WrongInstance);
        }
        state
            .epochs
            .iter_mut()
            .find(|e| e.journal.stream() == stream)
            .ok_or(Refusal::StaleEpoch)
    }
    fn live(state: &State, conn: ConnId) -> Result<(), Refusal> {
        if state.rpc_connections.contains(&conn) {
            Ok(())
        } else {
            Err(Refusal::WrongConnection)
        }
    }
    fn nonce(state: &mut State) -> Result<u64, Refusal> {
        let n = state.next;
        state.next = n.checked_add(1).ok_or(Refusal::ResourceLimit)?;
        Ok(n)
    }
    pub(crate) fn expire(&self, now: Instant) {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pending
            .retain(|p| now < p.expires);
    }
    pub(crate) fn can_retire(&self, key: &(String, u64)) -> bool {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        !state.epochs.iter().any(|e| {
            e.journal.stream().session.as_str() == key.0
                && e.journal.stream().epoch.0 == key.1
                && (e.claiming.is_some()
                    || e.owner.is_some()
                    || e.active.is_some()
                    || state
                        .pending
                        .iter()
                        .any(|p| p.stream == *e.journal.stream()))
        })
    }
    // Admission takes state before journal/session locks. Hold that same ordering
    // through the final journal cursor/reference check and registry removal.
    pub(crate) fn try_retire(
        &self,
        key: &(String, u64),
        output_retired: impl FnOnce() -> bool,
    ) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(epoch) = state.epochs.iter().find(|e| {
            e.journal.stream().session.as_str() == key.0 && e.journal.stream().epoch.0 == key.1
        }) && (epoch.claiming.is_some()
            || epoch.owner.is_some()
            || epoch.active.is_some()
            || state
                .pending
                .iter()
                .any(|p| p.stream == *epoch.journal.stream())
            || !epoch.journal.can_retire())
        {
            return false;
        }
        if !output_retired() {
            return false;
        }
        state.epochs.retain(|e| {
            e.journal.stream().session.as_str() != key.0 || e.journal.stream().epoch.0 != key.1
        });
        true
    }
    pub(crate) fn mutation(
        &self,
        conn: ConnId,
        stream: &StreamId,
        lease: ControllerLease,
    ) -> Result<(Arc<WriterHandle>, ControllerLease), Refusal> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        Self::live(&state, conn)?;
        let epoch = self.epoch(&mut state, stream)?;
        let owner = valid_owner(epoch, conn, lease)?;
        let session = epoch.session.upgrade().ok_or(Refusal::StaleEpoch)?;
        if session.cleanup_report().is_some() {
            return Err(Refusal::SessionFenced);
        }
        Ok((
            epoch.writer.upgrade().ok_or(Refusal::SessionFenced)?,
            owner.writer_lease.ok_or(Refusal::SessionFenced)?,
        ))
    }
    pub(crate) fn subscription_cursor(
        &self,
        conn: ConnId,
        stream: StreamId,
        lease: ControllerLease,
        after: Option<u64>,
    ) -> Result<Cursor, Refusal> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        Self::live(&state, conn)?;
        let epoch = self.epoch(&mut state, &stream)?;
        valid_owner(epoch, conn, lease)?;
        if epoch.active.is_some() {
            return Err(Refusal::Busy);
        }
        let acked = epoch.journal.position().0;
        let seq = match after {
            Some(seq) => seq,
            None if acked == 0 => 0,
            None => return Err(Refusal::CursorExpired),
        };
        Ok(Cursor { stream, seq })
    }
    pub(crate) async fn acknowledge(
        &self,
        conn: ConnId,
        cursor: Cursor,
        lease: ControllerLease,
    ) -> Result<(), Refusal> {
        let completion = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            Self::live(&state, conn)?;
            let epoch = self.epoch(&mut state, &cursor.stream)?;
            let owner = valid_owner(epoch, conn, lease)?;
            if cursor.seq <= owner.ceiling {
                // Published bytes remain acknowledgeable after stream finish.
                return reliable::acknowledge(&epoch.journal, cursor.seq);
            }
            Arc::clone(
                epoch
                    .final_write
                    .as_ref()
                    .filter(|pending| pending.seq == cursor.seq)
                    .ok_or(Refusal::FutureCursor)?,
            )
        };
        // Charge the actual inline timeout/wait future before constructing it.
        // Its Waker clones only retain already-admitted RPC task allocations.
        let metadata = reserve(
            &self.budget,
            ChargeKind::Metadata,
            std::mem::size_of::<tokio::time::Timeout<AckWait>>(),
        )?;
        let deadline = tokio::time::Instant::from_std(completion.deadline);
        let wait = AckWait {
            completion,
            slot: None,
            _metadata: metadata,
        };
        let _ = tokio::time::timeout_at(deadline, wait).await;
        // Both terminal notification and expiry require a fresh authority check.
        // Identity refusals precede FutureCursor, and pending is never credit.
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        Self::live(&state, conn)?;
        let epoch = self.epoch(&mut state, &cursor.stream)?;
        let owner = valid_owner(epoch, conn, lease)?;
        if cursor.seq > owner.ceiling {
            return Err(Refusal::FutureCursor);
        }
        reliable::acknowledge(&epoch.journal, cursor.seq)
    }
    pub(crate) fn begin_final_write<'a>(
        &'a self,
        cursor: &'a ReplayCursor,
        seq: u64,
        deadline: Instant,
    ) -> Result<Option<FinalWriteGuard<'a>>, Refusal> {
        let stream = cursor.authority.as_ref().ok_or(Refusal::StaleEpoch)?;
        let Some(lease) = stream.lease else {
            return Ok(None);
        };
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        Self::live(&state, stream.rpc)?;
        let epoch = self.epoch(&mut state, &stream.stream)?;
        valid_owner(epoch, stream.rpc, lease)?;
        if stream.state.cancelled()
            || !epoch
                .active
                .as_ref()
                .is_some_and(|active| Arc::ptr_eq(active, &stream.state))
        {
            return Err(Refusal::StaleLease);
        }
        if epoch.final_write.is_some() {
            return Err(Refusal::Busy);
        }
        let metadata = reserve(
            &self.budget,
            ChargeKind::Metadata,
            final_write_storage_bound(),
        )?;
        let completion = Arc::new(FinalWriteState {
            seq,
            deadline,
            settled: Mutex::new(Completion {
                terminal: false,
                waiters: std::array::from_fn(|_| None),
            }),
            _metadata: metadata,
        });
        epoch.final_write = Some(Arc::clone(&completion));
        Ok(Some(FinalWriteGuard {
            registry: self,
            stream,
            completion,
        }))
    }
    pub(crate) fn stop(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        for epoch in &mut state.epochs {
            cancel_stream(epoch);
        }
    }
    fn finish(&self, identity: &StreamId, active: &Arc<StreamState>) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Ok(epoch) = self.epoch(&mut state, identity)
            && epoch
                .active
                .as_ref()
                .is_some_and(|a| Arc::ptr_eq(a, active))
        {
            cancel_stream(epoch);
            epoch.active = None;
        }
    }
}
fn valid_owner(epoch: &Epoch, conn: ConnId, lease: ControllerLease) -> Result<Owner, Refusal> {
    let owner = epoch.owner.ok_or(Refusal::StaleLease)?;
    if owner.lease != lease {
        return Err(Refusal::StaleLease);
    }
    if owner.conn != conn {
        return Err(Refusal::WrongConnection);
    }
    Ok(owner)
}
// A generation without an owner is still unresolved admission across the writer
// fence. Cancellation and every error path retire it, without touching successors.
struct ClaimAdmission<'a> {
    reg: &'a AuthorityRegistry,
    stream: &'a StreamId,
    generation: u64,
}
impl Drop for ClaimAdmission<'_> {
    fn drop(&mut self) {
        let mut state = self.reg.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Ok(epoch) = self.reg.epoch(&mut state, self.stream)
            && epoch.claiming == Some(self.generation)
        {
            epoch.claiming = None;
        }
    }
}
pub(crate) async fn claim(
    reg: &AuthorityRegistry,
    conn: ConnId,
    stream: &StreamId,
) -> Result<ControllerGrant, Refusal> {
    let host = reg
        .context
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .upgrade()
        .ok_or(Refusal::Io)?;
    let (generation, writer, current) = {
        let mut state = reg.state.lock().unwrap_or_else(|e| e.into_inner());
        AuthorityRegistry::live(&state, conn)?;
        let generation = AuthorityRegistry::nonce(&mut state)?;
        let epoch = reg.epoch(&mut state, stream)?;
        let current = host
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(stream.session.as_str())
            .is_some_and(|session| {
                session.epoch == stream.epoch.0 && session.cleanup_report().is_none()
            });
        epoch.generation = generation;
        epoch.claiming = Some(generation);
        epoch.owner = None;
        cancel_stream(epoch);
        let writer = if current {
            epoch.writer.upgrade()
        } else {
            None
        };
        state.pending.retain(|p| p.stream != *stream);
        (generation, writer, current)
    };
    let _admission = ClaimAdmission {
        reg,
        stream,
        generation,
    };
    // Private fixture boundary: pause only the first armed claimant before enqueue.
    if let Some(dir) = crate::input::hold_dir()
        && dir.join("arm-reliable-pre-enqueue").exists()
        && std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(dir.join("selected-reliable-pre-enqueue"))
            .is_ok()
    {
        let deadline = Instant::now() + Duration::from_secs(5);
        while matches!(
            crate::input::poll_barrier(&dir, "reliable-pre-enqueue", Some(deadline)),
            crate::input::Barrier::Waiting
        ) {
            if !reg.connection_live(conn) {
                return Err(Refusal::WrongConnection);
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
    let writer_lease = if let Some(writer) = writer {
        let rx = {
            // Registry generation assignment and actual writer enqueue share the
            // same ordering lock. A superseded continuation cannot enqueue late.
            // The lock is released before waiting for the OS-write fence.
            let mut state = reg.state.lock().unwrap_or_else(|e| e.into_inner());
            AuthorityRegistry::live(&state, conn)?;
            if reg.epoch(&mut state, stream)?.generation != generation {
                return Err(Refusal::StaleLease);
            }
            writer
                .claim(conn.0, stream.epoch.0)
                .map_err(|_| Refusal::SessionFenced)?
        };
        Some(ControllerLease(
            rx.await
                .map_err(|_| Refusal::Io)?
                .map_err(|_| Refusal::SessionFenced)?,
        ))
    } else {
        None
    };
    let mut state = reg.state.lock().unwrap_or_else(|e| e.into_inner());
    AuthorityRegistry::live(&state, conn)?;
    let lease = ControllerLease(AuthorityRegistry::nonce(&mut state)?);
    let epoch = reg.epoch(&mut state, stream)?;
    if epoch.generation != generation {
        return Err(Refusal::StaleLease);
    }
    if current
        && !host
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(stream.session.as_str())
            .is_some_and(|session| {
                session.epoch == stream.epoch.0 && session.cleanup_report().is_none()
            })
    {
        return Err(Refusal::StaleEpoch);
    }
    let (acked, latest) = epoch.journal.position();
    epoch.owner = Some(Owner {
        conn,
        lease,
        writer_lease,
        ceiling: acked,
    });
    Ok(ControllerGrant {
        lease,
        acked,
        latest,
    })
}
pub(crate) fn issue_ticket(
    reg: &AuthorityRegistry,
    conn: ConnId,
    cursor: Cursor,
    lease: Option<ControllerLease>,
    observer: bool,
    now: Instant,
) -> Result<TicketGrant, Refusal> {
    let mut state = reg.state.lock().unwrap_or_else(|e| e.into_inner());
    AuthorityRegistry::live(&state, conn)?;
    state.pending.retain(|p| now < p.expires);
    let epoch = reg.epoch(&mut state, &cursor.stream)?;
    if !observer {
        valid_owner(epoch, conn, lease.ok_or(Refusal::StaleLease)?)?;
        if epoch.active.is_some() {
            return Err(Refusal::Busy);
        }
    }
    epoch.journal.validate_cursor(&cursor)?;
    let journal = Arc::clone(&epoch.journal);
    // Bounded ticket replacement can free only the old pending (never active) reservation.
    state
        .pending
        .retain(|p| !(p.rpc == conn && p.lease.is_some() && !observer));
    if state.pending.len() >= 16 {
        return Err(Refusal::ResourceLimit);
    }
    let slot = reg.hub.reserve_slot()?;
    let metadata = reserve(
        &reg.budget,
        ChargeKind::Metadata,
        std::mem::size_of::<StreamState>() + ARC_OVERHEAD + std::mem::size_of::<StreamGuard>() + 64,
    )?;
    let (watermark, replay) = reliable::begin_replay(&journal, cursor.seq)?;
    let random = AuthToken::generate().map_err(|_| Refusal::Io)?;
    let token = Ticket::new(*random.as_bytes());
    let (acked, latest) = journal.position();
    let epoch = reg.epoch(&mut state, &cursor.stream)?;
    if !observer {
        epoch.owner.as_mut().expect("validated owner").ceiling =
            epoch.owner.unwrap().ceiling.max(cursor.seq);
    }
    state.pending.push(Pending {
        token: token.clone(),
        rpc: conn,
        stream: cursor.stream,
        lease: if observer { None } else { lease },
        expires: now + TICKET_LIFETIME,
        cursor: replay,
        slot,
        metadata,
    });
    Ok(TicketGrant {
        ticket: token,
        acked,
        latest,
        watermark,
    })
}
pub(crate) fn consume_ticket(
    reg: &AuthorityRegistry,
    event_conn: ConnId,
    ticket: Ticket,
    now: Instant,
) -> Result<ReplayCursor, Refusal> {
    let mut state = reg.state.lock().unwrap_or_else(|e| e.into_inner());
    AuthorityRegistry::live(&state, event_conn)?;
    state.pending.retain(|p| now < p.expires);
    let index = state
        .pending
        .iter()
        .position(|p| p.token == ticket)
        .ok_or(Refusal::TicketInvalid)?;
    // Consuming this ticket turns event_conn into an event-only connection.
    // It cannot simultaneously remain the separate live RPC that owns the ticket.
    if state.pending[index].rpc == event_conn {
        return Err(Refusal::TicketInvalid);
    }
    let pending = state.pending.remove(index);
    AuthorityRegistry::live(&state, pending.rpc).map_err(|_| Refusal::TicketInvalid)?;
    let epoch = reg.epoch(&mut state, &pending.stream)?;
    if let Some(lease) = pending.lease {
        valid_owner(epoch, pending.rpc, lease).map_err(|_| Refusal::TicketInvalid)?;
        if epoch.active.is_some() {
            return Err(Refusal::Busy);
        }
    }
    let active = Arc::new(StreamState {
        cancelled: AtomicBool::new(false),
    });
    // Observers do not occupy/mutate the authoritative active slot.
    if pending.lease.is_some() {
        epoch.active = Some(Arc::clone(&active));
    }
    let context = reg
        .context
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .upgrade()
        .ok_or(Refusal::Io)?;
    let mut cursor = pending.cursor;
    cursor.authority = Some(StreamGuard {
        registry: Arc::downgrade(&context.authority),
        state: active,
        stream: pending.stream,
        rpc: pending.rpc,
        lease: pending.lease,
        slot: Some(pending.slot),
        metadata: Some(pending.metadata),
    });
    Ok(cursor)
}
pub(crate) fn revoke_connection(reg: &AuthorityRegistry, conn: ConnId) {
    let mut state = reg.state.lock().unwrap_or_else(|e| e.into_inner());
    state.rpc_connections.retain(|c| *c != conn);
    state.pending.retain(|p| p.rpc != conn);
    for epoch in &mut state.epochs {
        if epoch.owner.is_some_and(|owner| owner.conn == conn) {
            epoch.owner = None;
            cancel_stream(epoch);
        }
    }
}
impl TicketGrant {
    pub(crate) fn response(self) -> ExtensionResponse {
        ExtensionResponse::Subscription {
            ticket: self.ticket,
            acked: self.acked,
            oldest: self.acked.saturating_add(1),
            latest: self.latest,
            watermark: self.watermark,
        }
    }
}

#[cfg(test)]
mod rendezvous_tests {
    use super::*;
    use crate::slice_budget::METADATA;
    use fl_host_proto::{SessionEpoch, SessionId};
    use std::sync::atomic::AtomicUsize;
    use std::task::Wake;

    struct WakeCount(AtomicUsize);
    impl Wake for WakeCount {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
        fn wake_by_ref(self: &Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    struct Fixture {
        reg: Arc<AuthorityRegistry>,
        journal: Arc<ReliableJournal>,
        replay: ReplayCursor,
        stream: StreamId,
        lease: ControllerLease,
    }
    impl Fixture {
        fn new(observer: bool) -> Self {
            let budget = SliceBudget::new();
            let reg = AuthorityRegistry::new(Arc::clone(&budget), Arc::new(Hub::new())).unwrap();
            reg.connect(ConnId(1)).unwrap();
            reg.connect(ConnId(2)).unwrap();
            let stream = StreamId {
                host: reg.instance().clone(),
                session: SessionId::new("rendezvous").unwrap(),
                epoch: SessionEpoch(1),
            };
            let journal = ReliableJournal::new(budget, stream.clone()).unwrap();
            let mut bytes = journal.allocate(1).unwrap();
            bytes.as_mut_slice().copy_from_slice(b"x");
            assert_eq!(
                reliable::append(&journal, crate::reliable::ReliableBody::Line(bytes)).unwrap(),
                1
            );
            let prepared = reg.prepare_epoch(Arc::clone(&journal)).unwrap();
            let stream_metadata = reserve(
                &reg.budget,
                ChargeKind::Metadata,
                std::mem::size_of::<StreamState>()
                    + ARC_OVERHEAD
                    + std::mem::size_of::<StreamGuard>()
                    + 64,
            )
            .unwrap();
            let active = Arc::new(StreamState {
                cancelled: AtomicBool::new(false),
            });
            let lease = ControllerLease(7);
            reg.state.lock().unwrap().epochs.push(Epoch {
                journal: Arc::clone(&journal),
                session: Weak::new(),
                writer: Weak::new(),
                generation: 1,
                claiming: None,
                owner: Some(Owner {
                    conn: ConnId(1),
                    lease,
                    writer_lease: None,
                    ceiling: 0,
                }),
                active: if observer {
                    None
                } else {
                    Some(Arc::clone(&active))
                },
                final_write: None,
                _metadata: prepared.metadata,
            });
            let (_, mut replay) = reliable::begin_replay(&journal, 0).unwrap();
            replay.authority = Some(StreamGuard {
                registry: Arc::downgrade(&reg),
                state: active,
                stream: stream.clone(),
                rpc: ConnId(1),
                lease: if observer { None } else { Some(lease) },
                slot: None,
                metadata: Some(stream_metadata),
            });
            Self {
                reg,
                journal,
                replay,
                stream,
                lease,
            }
        }
        fn cursor(&self, seq: u64) -> Cursor {
            Cursor {
                stream: self.stream.clone(),
                seq,
            }
        }
        fn ack(&self, seq: u64) -> impl Future<Output = Result<(), Refusal>> + '_ {
            self.reg
                .acknowledge(ConnId(1), self.cursor(seq), self.lease)
        }
    }
    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }
    fn pending<F: Future>(future: Pin<&mut F>, waker: &Waker) {
        assert!(matches!(
            future.poll(&mut Context::from_waker(waker)),
            Poll::Pending
        ));
    }

    // Catches precredit, missing completion wake, and requiring a finished stream for published Ack.
    #[test]
    fn exact_final_write_waits_then_published_ack_survives_stream_finish() {
        runtime().block_on(async {
            let mut f = Fixture::new(false);
            let final_write = f
                .reg
                .begin_final_write(&f.replay, 1, Instant::now() + Duration::from_secs(5))
                .unwrap()
                .unwrap();
            let wake = Arc::new(WakeCount(AtomicUsize::new(0)));
            let waker = Waker::from(Arc::clone(&wake));
            let mut ack = Box::pin(f.ack(1));
            pending(ack.as_mut(), &waker);
            assert_eq!(
                f.journal.position().0,
                0,
                "guessing the exact in-flight seq grants no credit"
            );
            assert_eq!(f.ack(2).await, Err(Refusal::FutureCursor));
            assert_eq!(
                f.reg.acknowledge(ConnId(2), f.cursor(1), f.lease).await,
                Err(Refusal::WrongConnection)
            );
            assert_eq!(
                f.reg
                    .acknowledge(ConnId(1), f.cursor(1), ControllerLease(99))
                    .await,
                Err(Refusal::StaleLease)
            );
            final_write.complete();
            assert_eq!(wake.0.load(Ordering::SeqCst), 1);
            assert_eq!(ack.await, Ok(()));
            f.replay.authority.take();
            assert!(f.reg.state.lock().unwrap().epochs[0].active.is_none());
            assert_eq!(
                f.ack(1).await,
                Ok(()),
                "published duplicate is legal after stream finish"
            );
            assert_eq!(f.journal.position().0, 1);
        });
    }
    // Catches earlier-fragment/observer credit and final-write task-drop leaking pending Ack.
    #[test]
    fn no_final_token_or_failed_final_write_cannot_retire_records() {
        runtime().block_on(async {
            let f = Fixture::new(false);
            assert_eq!(
                f.ack(1).await,
                Err(Refusal::FutureCursor),
                "partial earlier fragment has no token"
            );
            let final_write = f
                .reg
                .begin_final_write(&f.replay, 1, Instant::now() + Duration::from_secs(5))
                .unwrap()
                .unwrap();
            let mut ack = Box::pin(f.ack(1));
            pending(ack.as_mut(), Waker::noop());
            drop(final_write);
            assert_eq!(ack.await, Err(Refusal::FutureCursor));
            assert_eq!(f.journal.position(), (0, 1));
            let f = Fixture::new(true);
            assert!(
                f.reg
                    .begin_final_write(&f.replay, 1, Instant::now() + Duration::from_secs(5))
                    .unwrap()
                    .is_none()
            );
            assert_eq!(f.ack(1).await, Err(Refusal::FutureCursor));
            assert_eq!(f.journal.position(), (0, 1));
        });
    }
    // Catches missed cancellation wakes, stale completion credit and identity refusal precedence.
    #[test]
    fn disconnect_takeover_and_stop_wake_without_successor_credit() {
        runtime().block_on(async {
            for cause in ["disconnect", "takeover", "stop"] {
                let f = Fixture::new(false);
                let final_write = f
                    .reg
                    .begin_final_write(&f.replay, 1, Instant::now() + Duration::from_secs(5))
                    .unwrap()
                    .unwrap();
                let wake = Arc::new(WakeCount(AtomicUsize::new(0)));
                let waker = Waker::from(Arc::clone(&wake));
                let mut ack = Box::pin(f.ack(1));
                pending(ack.as_mut(), &waker);
                let expected = match cause {
                    "disconnect" => {
                        revoke_connection(&f.reg, ConnId(1));
                        Refusal::WrongConnection
                    }
                    "takeover" => {
                        let mut state = f.reg.state.lock().unwrap();
                        let epoch = f.reg.epoch(&mut state, &f.stream).unwrap();
                        cancel_stream(epoch);
                        epoch.owner = Some(Owner {
                            conn: ConnId(2),
                            lease: ControllerLease(8),
                            writer_lease: None,
                            ceiling: 0,
                        });
                        Refusal::StaleLease
                    }
                    "stop" => {
                        f.reg.stop();
                        Refusal::FutureCursor
                    }
                    _ => unreachable!(),
                };
                assert_eq!(wake.0.load(Ordering::SeqCst), 1, "{cause}");
                assert_eq!(ack.await, Err(expected), "{cause}");
                final_write.complete();
                assert_eq!(f.journal.position(), (0, 1), "{cause}");
                assert_eq!(
                    f.reg.state.lock().unwrap().epochs[0]
                        .owner
                        .map(|o| o.ceiling),
                    if cause == "disconnect" { None } else { Some(0) }
                );
            }
        });
    }
    // Catches deadline renewal, credit after expiration, and late identity checks losing precedence.
    #[test]
    fn final_write_and_wait_use_one_expired_absolute_deadline() {
        runtime().block_on(async {
            for disconnect in [false, true] {
                let f = Fixture::new(false);
                let deadline = Instant::now() - Duration::from_secs(1);
                let final_write = f
                    .reg
                    .begin_final_write(&f.replay, 1, deadline)
                    .unwrap()
                    .unwrap();
                let completion = f.reg.state.lock().unwrap().epochs[0]
                    .final_write
                    .as_ref()
                    .unwrap()
                    .clone();
                assert_eq!(completion.deadline, deadline);
                if disconnect {
                    revoke_connection(&f.reg, ConnId(1));
                }
                let mut ack = Box::pin(f.ack(1));
                assert_eq!(
                    ack.as_mut().poll(&mut Context::from_waker(Waker::noop())),
                    Poll::Ready(Err(if disconnect {
                        Refusal::WrongConnection
                    } else {
                        Refusal::FutureCursor
                    })),
                    "an already-expired absolute deadline cannot begin another wait"
                );
                drop(ack);
                final_write.complete();
                assert_eq!(f.journal.position(), (0, 1));
                assert_eq!(
                    f.reg.state.lock().unwrap().epochs[0]
                        .owner
                        .map(|o| o.ceiling),
                    if disconnect { None } else { Some(0) }
                );
            }
        });
    }
    // Catches metadata release while a waiter retains completion and stale unregister clobbering a slot.
    #[test]
    fn completion_and_waiters_remain_charged_through_last_reference() {
        runtime().block_on(async {
            let f = Fixture::new(false);
            let base = f.reg.budget.used(ChargeKind::Metadata);
            let final_write = f
                .reg
                .begin_final_write(&f.replay, 1, Instant::now() + Duration::from_secs(5))
                .unwrap()
                .unwrap();
            let token_charge = final_write_storage_bound();
            let wait_charge = std::mem::size_of::<tokio::time::Timeout<AckWait>>();
            eprintln!("completion storage={token_charge}, wait storage={wait_charge}, completion allocation={}, Arc header={ARC_OVERHEAD}, guard={}, registry handle={}",
                std::mem::size_of::<FinalWriteState>(), std::mem::size_of::<FinalWriteGuard<'_>>(), std::mem::size_of::<Arc<FinalWriteState>>());
            assert_eq!(f.reg.budget.used(ChargeKind::Metadata), base + token_charge);
            let mut first = Box::pin(f.ack(1));
            pending(first.as_mut(), Waker::noop());
            assert_eq!(
                f.reg.budget.used(ChargeKind::Metadata),
                base + token_charge + wait_charge
            );
            drop(first);
            assert_eq!(f.reg.budget.used(ChargeKind::Metadata), base + token_charge);
            let mut waits: Vec<_> = (0..64).map(|_| Box::pin(f.ack(1))).collect();
            for wait in &mut waits {
                pending(wait.as_mut(), Waker::noop());
            }
            assert_eq!(
                f.reg.budget.used(ChargeKind::Metadata),
                base + token_charge + 64 * wait_charge
            );
            drop(final_write);
            assert_eq!(
                f.reg.budget.used(ChargeKind::Metadata),
                base + token_charge + 64 * wait_charge,
                "registry/producer retirement cannot free waiter-retained allocation"
            );
            for wait in waits {
                assert_eq!(wait.await, Err(Refusal::FutureCursor));
            }
            assert_eq!(f.reg.budget.used(ChargeKind::Metadata), base);
        });
    }
    // Catches allocating a completion or waiter before aggregate metadata admission succeeds.
    #[test]
    fn exhausted_metadata_refuses_completion_and_wait_before_allocation() {
        runtime().block_on(async {
            let f = Fixture::new(false);
            let base = f.reg.budget.used(ChargeKind::Metadata);
            let filler = reserve(&f.reg.budget, ChargeKind::Metadata, METADATA - base).unwrap();
            assert!(matches!(
                f.reg
                    .begin_final_write(&f.replay, 1, Instant::now() + Duration::from_secs(5)),
                Err(Refusal::ResourceLimit)
            ));
            assert!(f.reg.state.lock().unwrap().epochs[0].final_write.is_none());
            drop(filler);
            let final_write = f
                .reg
                .begin_final_write(&f.replay, 1, Instant::now() + Duration::from_secs(5))
                .unwrap()
                .unwrap();
            let used = f.reg.budget.used(ChargeKind::Metadata);
            let _filler = reserve(&f.reg.budget, ChargeKind::Metadata, METADATA - used).unwrap();
            assert_eq!(f.ack(1).await, Err(Refusal::ResourceLimit));
            assert_eq!(f.reg.budget.used(ChargeKind::Metadata), METADATA);
            assert_eq!(f.journal.position(), (0, 1));
            drop(final_write);
        });
    }
}
