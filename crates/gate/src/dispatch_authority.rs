//! Fail-closed, process-local permit and revocation state machine.
//!
//! This module alone is **not** a deployed authority: it has no authenticated
//! host transport, ledger verifier, or broker singleton. The embedding host must
//! install an authenticated source and restrict completion to the permit holder.
//! `UnavailableSource` is the production default until those boundaries exist.
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::GateError;
use ring::rand::{SecureRandom, SystemRandom};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum ScopeKey {
    Secret(String),
    Bundle(String),
    Incarnation(String),
}
impl ScopeKey {
    fn valid(&self) -> bool {
        let s = match self {
            Self::Secret(s) | Self::Bundle(s) | Self::Incarnation(s) => s,
        };
        !s.trim().is_empty() && s.len() <= 1024 && !s.chars().any(char::is_control)
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdmitRequest {
    pub secret: String,
    pub bundle: Option<String>,
    pub incarnation: Option<String>,
}
impl AdmitRequest {
    pub fn secret(name: &str) -> Self {
        Self {
            secret: name.into(),
            bundle: None,
            incarnation: None,
        }
    }
    fn required_scopes(&self) -> Option<BTreeSet<ScopeKey>> {
        let valid =
            |s: &str| !s.trim().is_empty() && s.len() <= 1024 && !s.chars().any(char::is_control);
        if !valid(&self.secret)
            || self.bundle.as_deref().is_some_and(|s| !valid(s))
            || self.incarnation.as_deref().is_some_and(|s| !valid(s))
        {
            return None;
        }
        let mut out = BTreeSet::from([ScopeKey::Secret(self.secret.clone())]);
        match (&self.bundle, &self.incarnation) {
            (Some(bundle), Some(incarnation)) => {
                out.insert(ScopeKey::Bundle(bundle.clone()));
                out.insert(ScopeKey::Incarnation(incarnation.clone()));
            }
            (None, None) => {}
            _ => return None,
        }
        out.iter().all(ScopeKey::valid).then_some(out)
    }
}
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct PermitId([u8; 16]);
impl PermitId {
    /// Canonical transport encoding; possession of this identifier is not authorization.
    pub fn to_wire(&self) -> String {
        use std::fmt::Write;
        let mut out = String::with_capacity(32);
        for b in self.0 {
            write!(out, "{b:02x}").expect("writing to String cannot fail");
        }
        out
    }
    /// Parse exactly 32 lowercase hexadecimal digits.
    pub fn from_wire(input: &str) -> Option<Self> {
        let bytes = input.as_bytes();
        if bytes.len() != 32
            || !bytes
                .iter()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(c))
        {
            return None;
        }
        let mut out = [0; 16];
        for (dst, pair) in out.iter_mut().zip(bytes.as_chunks::<2>().0) {
            let hex = |b: u8| {
                if b.is_ascii_digit() {
                    b - b'0'
                } else {
                    b - b'a' + 10
                }
            };
            *dst = (hex(pair[0]) << 4) | hex(pair[1]);
        }
        Some(Self(out))
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerIdentity {
    pub pid: u32,
    pub start_identity: u64,
}
/// A termination observation authenticated by the configured trusted host verifier.
/// This value is evidence only when returned by that verifier; caller-supplied values
/// must never be accepted by a host transport as proof.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedTermination {
    pub worker: WorkerIdentity,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Permit {
    pub id: PermitId,
    pub deadline_mono_ns: u64,
    pub record_epoch: String,
    pub resources: Vec<String>,
}
impl Permit {
    pub fn remaining(&self, clock: &dyn MonotonicClock) -> Result<Duration, GateError> {
        let remaining = self
            .deadline_mono_ns
            .checked_sub(clock.now_ns()?)
            .filter(|n| *n > 0)
            .ok_or(GateError::DispatchAuthority("permit expired"))?;
        Ok(Duration::from_nanos(remaining))
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedAdmission {
    /// Authenticated state revision; all changes to grant/revocation state must advance it.
    pub generation: u64,
    pub scope: BTreeSet<ScopeKey>,
    pub record_epoch: String,
    pub resources: Vec<String>,
    /// Obtained and authenticated by the host, never from request text.
    pub worker: WorkerIdentity,
}
/// A non-I/O guard supplied by an authoritative source after authentication. While this
/// guard is alive, every sanctioned mutation of the guarded scopes must be fenced from
/// taking effect or advancing their authority generation. The implementation must not hold
/// `VaultLock`, perform I/O, or wait on broker operations while the broker mutex is held.
/// This is a trusted source contract; the in-process kernel cannot enforce it across a
/// separately running writer. No production guarded source is provided by this module.
pub trait AdmissionGenerationGuard {
    /// The current authenticated generation held stable by this guard.
    fn generation(&self) -> u64;
}

/// Trusted host adapter. It must authenticate all evidence, expand aliases, ensure
/// revisions cannot be replayed, and fence pre-crash grants on every reconcile.
/// This trait is a trust boundary, not a public proof that values are authenticated.
pub trait AuthoritySource: Send + Sync {
    fn reconcile(&self) -> Result<u64, GateError>;
    fn verify_admission(&self, request: &AdmitRequest) -> Result<VerifiedAdmission, GateError>;
    /// Establish a scope-local, non-I/O mutation fence and report the generation that is
    /// current under that fence. The default refuses: comparing a cached generation without
    /// coordinating every sanctioned writer is not a freshness guarantee. An implementation
    /// must cover every verified alias and keep the generation stable until the guard drops.
    /// An empty scope is a reconciliation fence and must cover the complete source state.
    fn guard_admission<'a>(
        &'a self,
        _scope: &BTreeSet<ScopeKey>,
    ) -> Result<Box<dyn AdmissionGenerationGuard + 'a>, GateError> {
        Err(GateError::DispatchAuthority(
            "coordinated source unavailable",
        ))
    }
    fn resolve_revocation(
        &self,
        requested: &BTreeSet<ScopeKey>,
    ) -> Result<(u64, BTreeSet<ScopeKey>), GateError>;
    fn persist_fence(&self, scope: &BTreeSet<ScopeKey>, reason: &str) -> Result<(), GateError>;
    /// Reauthenticate the durable authority snapshot after persistence, and confirm that
    /// it denies every requested scope. Return that snapshot's monotonically increasing
    /// generation. This performs I/O outside the broker mutex. The caller subsequently
    /// holds the whole-source mutation guard and compares its generation before relying on
    /// this result. A source must coordinate every sanctioned writer with that guard.
    /// Persistence and verification must be idempotent: a failed verification can be retried
    /// after the durable write succeeded. The default refuses; an unverified write receipt
    /// cannot authorize retirement of an in-memory fence.
    fn verify_persisted_fence(&self, _scope: &BTreeSet<ScopeKey>) -> Result<u64, GateError> {
        Err(GateError::DispatchAuthority(
            "durable fence verification unavailable",
        ))
    }
    /// Authenticate the complete generation history in `(from, to]` and prove
    /// that every change is a monotonic durable revocation of one of `pending`.
    /// The implementation must bound work and fail closed on an oversized or
    /// incomplete history. A matching current generation or the continued presence of one fence is
    /// not enough: an intervening policy change or grant must cause refusal.
    /// This may perform I/O and is called without the broker mutex. The caller
    /// must subsequently acquire a whole-source guard and recheck `to`.
    fn verify_revocation_transition(
        &self,
        _from: u64,
        _to: u64,
        _pending: &[BTreeSet<ScopeKey>],
    ) -> Result<(), GateError> {
        Err(GateError::DispatchAuthority(
            "revocation transition verification unavailable",
        ))
    }
    fn worker_terminated(
        &self,
        worker: &WorkerIdentity,
    ) -> Result<Option<VerifiedTermination>, GateError>;
    /// Request cancellation only; success is not a quiescence receipt.
    fn request_cancellation(&self, id: &PermitId, worker: &WorkerIdentity)
    -> Result<(), GateError>;
}
#[derive(Debug, Default)]
pub struct UnavailableSource;
impl AuthoritySource for UnavailableSource {
    fn reconcile(&self) -> Result<u64, GateError> {
        Err(GateError::DispatchAuthority("authority unavailable"))
    }
    fn verify_admission(&self, _: &AdmitRequest) -> Result<VerifiedAdmission, GateError> {
        Err(GateError::DispatchAuthority("authority unavailable"))
    }
    fn resolve_revocation(
        &self,
        _: &BTreeSet<ScopeKey>,
    ) -> Result<(u64, BTreeSet<ScopeKey>), GateError> {
        Err(GateError::DispatchAuthority("authority unavailable"))
    }
    fn persist_fence(&self, _: &BTreeSet<ScopeKey>, _: &str) -> Result<(), GateError> {
        Err(GateError::DispatchAuthority("authority unavailable"))
    }
    fn worker_terminated(
        &self,
        _: &WorkerIdentity,
    ) -> Result<Option<VerifiedTermination>, GateError> {
        Err(GateError::DispatchAuthority("authority unavailable"))
    }
    fn request_cancellation(&self, _: &PermitId, _: &WorkerIdentity) -> Result<(), GateError> {
        Err(GateError::DispatchAuthority("authority unavailable"))
    }
}
pub trait MonotonicClock: Send + Sync {
    fn now_ns(&self) -> Result<u64, GateError>;
}
#[derive(Debug, Default)]
pub struct HostMonotonicClock;
#[cfg(unix)]
impl MonotonicClock for HostMonotonicClock {
    fn now_ns(&self) -> Result<u64, GateError> {
        let mut value = std::mem::MaybeUninit::<libc::timespec>::uninit();
        // SAFETY: clock_gettime initializes `value` on success and pointer is valid.
        if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, value.as_mut_ptr()) } != 0 {
            return Err(GateError::DispatchAuthority("monotonic clock unavailable"));
        }
        // SAFETY: successful clock_gettime initialized the timespec.
        let value = unsafe { value.assume_init() };
        let secs = u64::try_from(value.tv_sec)
            .map_err(|_| GateError::DispatchAuthority("invalid monotonic clock"))?;
        let nanos = u64::try_from(value.tv_nsec)
            .map_err(|_| GateError::DispatchAuthority("invalid monotonic clock"))?;
        if nanos >= 1_000_000_000 {
            return Err(GateError::DispatchAuthority("invalid monotonic clock"));
        }
        secs.checked_mul(1_000_000_000)
            .and_then(|s| s.checked_add(nanos))
            .ok_or(GateError::DispatchAuthority("monotonic clock overflow"))
    }
}
#[cfg(not(unix))]
impl MonotonicClock for HostMonotonicClock {
    fn now_ns(&self) -> Result<u64, GateError> {
        Err(GateError::DispatchAuthority(
            "host monotonic clock unsupported",
        ))
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthorityHealth {
    Sealed,
    Ready,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevokeAck {
    Complete,
    PendingUnconfirmed,
}
pub trait DispatchAuthority: Send + Sync {
    fn admit(&self, request: &AdmitRequest) -> Result<Permit, GateError>;
    fn complete(&self, id: &PermitId) -> Result<(), GateError>;
    fn revoke(
        &self,
        scope: &BTreeSet<ScopeKey>,
        reason: &str,
        wait: Duration,
    ) -> Result<RevokeAck, GateError>;
    fn health(&self) -> Result<AuthorityHealth, GateError>;
    fn reconcile(&self) -> Result<(), GateError>;
}
#[derive(Debug)]
struct LivePermit {
    scope: BTreeSet<ScopeKey>,
    worker: WorkerIdentity,
    deadline_mono_ns: u64,
    cancellation_requested: bool,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FencePhase {
    Pending,
    Persisting,
    Persisted,
    Failed,
}
#[derive(Debug)]
struct Fence {
    scope: BTreeSet<ScopeKey>,
    reason: String,
    phase: FencePhase,
    /// The revoke call still needs to observe the final result. A timed-out caller
    /// relinquishes this; a later completion may then retire the verified fence.
    waiting: bool,
}
#[derive(Default)]
struct State {
    generation: Option<u64>,
    reconciled: bool,
    /// A revocation whose scope could not be established must never be erased by
    /// an in-progress initial reconciliation.
    revocation_failed: bool,
    live: BTreeMap<PermitId, LivePermit>,
    fences: BTreeMap<u64, Fence>,
    next_fence_id: u64,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RefreshResult {
    Published,
    Retry,
    Unverified,
}
/// A pure in-process kernel; singleton hosting, authenticated transport, and durable
/// source are deliberately not implemented here. Starts sealed after every restart.
pub struct BrokerState {
    state: Mutex<State>,
    changed: Condvar,
    source: Arc<dyn AuthoritySource>,
    clock: Arc<dyn MonotonicClock>,
    operation_ns: u64,
}
impl BrokerState {
    pub fn unavailable() -> Self {
        Self::new(
            Arc::new(UnavailableSource),
            Arc::new(HostMonotonicClock),
            Duration::from_secs(30),
        )
        .expect("valid default")
    }
    pub fn new(
        source: Arc<dyn AuthoritySource>,
        clock: Arc<dyn MonotonicClock>,
        timeout: Duration,
    ) -> Result<Self, GateError> {
        let operation_ns = u64::try_from(timeout.as_nanos())
            .ok()
            .filter(|n| *n > 0)
            .ok_or(GateError::DispatchAuthority("invalid operation timeout"))?;
        Ok(Self {
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
            source,
            clock,
            operation_ns,
        })
    }
    fn state(&self) -> Result<std::sync::MutexGuard<'_, State>, GateError> {
        self.state
            .lock()
            .map_err(|_| GateError::DispatchAuthority("authority poisoned"))
    }
    fn ready_to_persist(s: &mut State) -> Vec<(u64, BTreeSet<ScopeKey>, String)> {
        let mut ready = Vec::new();
        for (id, fence) in &mut s.fences {
            if fence.phase == FencePhase::Pending
                && !s.live.values().any(|p| !p.scope.is_disjoint(&fence.scope))
            {
                fence.phase = FencePhase::Persisting;
                ready.push((*id, fence.scope.clone(), fence.reason.clone()));
            }
        }
        ready
    }
    fn persist_ready(
        &self,
        ready: Vec<(u64, BTreeSet<ScopeKey>, String)>,
    ) -> Result<(), GateError> {
        let mut failed = false;
        for (id, scope, reason) in ready {
            // No broker lock is held during external storage work.
            let persisted = self.source.persist_fence(&scope, &reason).is_ok();
            let ok = persisted && self.verify_and_retire(id, &scope)?;
            if ok {
                continue;
            }
            let mut state = self.state()?;
            let Some(fence) = state.fences.get_mut(&id) else {
                return Err(GateError::DispatchAuthority("revocation fence missing"));
            };
            failed = true;
            fence.phase = FencePhase::Failed;
            self.changed.notify_all();
        }
        if failed {
            Err(GateError::DispatchAuthority(
                "revocation persistence failed",
            ))
        } else {
            Ok(())
        }
    }

    /// Authenticate the persisted fence, then hold a whole-source guard while
    /// publishing its generation and retiring the in-memory fence. Another writer
    /// may advance the source between authentication and the guard; retry that
    /// race a bounded number of times without repeating the durable write.
    fn verify_and_retire(&self, id: u64, scope: &BTreeSet<ScopeKey>) -> Result<bool, GateError> {
        for _ in 0..8 {
            let Ok(generation) = self.source.verify_persisted_fence(scope) else {
                return Ok(false);
            };
            let (from, pending): (_, Vec<_>) = {
                let state = self.state()?;
                let Some(current) = state.generation.filter(|_| !state.revocation_failed) else {
                    return Ok(false);
                };
                let pending = state
                    .fences
                    .values()
                    .filter(|f| f.phase == FencePhase::Persisting)
                    .map(|f| f.scope.clone())
                    .collect();
                (current, pending)
            };
            if generation < from {
                continue;
            }
            if generation > from
                && self
                    .source
                    .verify_revocation_transition(from, generation, &pending)
                    .is_err()
            {
                // A concurrent authenticated publication or source mutation can
                // invalidate the snapshot while it is being checked. Retry rather
                // than treating that transient mismatch as a verified failure.
                if self.state()?.generation != Some(from) {
                    continue;
                }
                if let Ok(guard) = self.source.guard_admission(&BTreeSet::new())
                    && guard.generation() > generation
                {
                    continue;
                }
                return Ok(false);
            }
            let Ok(guard) = self.source.guard_admission(&BTreeSet::new()) else {
                return Ok(false);
            };
            if generation == 0 || guard.generation() != generation {
                continue;
            }
            let mut state = self.state()?;
            if state.revocation_failed || state.generation.is_none() {
                return Ok(false);
            }
            if state.generation != Some(from) {
                continue;
            }
            state.generation = Some(generation);
            let Some(fence) = state.fences.get_mut(&id) else {
                return Err(GateError::DispatchAuthority("revocation fence missing"));
            };
            fence.phase = FencePhase::Persisted;
            if !fence.waiting {
                state.fences.remove(&id);
            }
            self.changed.notify_all();
            return Ok(true);
        }
        Ok(false)
    }

    /// Publish an authenticated source generation while another fence's durable write
    /// is being verified. The existing in-memory fence stays in place: verifying it here
    /// is evidence for the source snapshot, not permission to retire that fence.
    ///
    /// Source I/O is outside the broker mutex. A whole-source guard then makes the
    /// generation recheck and publication atomic with respect to sanctioned writers.
    fn refresh_during_persistence(&self, expected: u64) -> Result<RefreshResult, GateError> {
        let (from, witnesses): (_, Vec<_>) = {
            let state = self.state()?;
            if state.generation == Some(expected) && !state.revocation_failed {
                return Ok(RefreshResult::Published);
            }
            if state.generation.is_some_and(|current| current > expected)
                && !state.revocation_failed
            {
                return Ok(RefreshResult::Retry);
            }
            let Some(from) = state.generation.filter(|_| !state.revocation_failed) else {
                return Ok(RefreshResult::Unverified);
            };
            let witnesses = state
                .fences
                .iter()
                .filter(|(_, fence)| fence.phase == FencePhase::Persisting)
                .take(4096)
                .map(|(id, fence)| (*id, fence.scope.clone()))
                .collect();
            (from, witnesses)
        };
        if expected <= from || witnesses.is_empty() {
            return Ok(RefreshResult::Unverified);
        }
        let pending = witnesses
            .iter()
            .map(|(_, scope)| scope.clone())
            .collect::<Vec<_>>();
        if self
            .source
            .verify_revocation_transition(from, expected, &pending)
            .is_err()
        {
            // The authenticated source can advance during verification. Re-resolve
            // the request instead of sealing solely on that transient race; a
            // subsequent attempt still has to prove the entire new transition.
            if let Ok(guard) = self.source.guard_admission(&BTreeSet::new())
                && guard.generation() > expected
            {
                return Ok(RefreshResult::Retry);
            }
            let state = self.state()?;
            if state.generation == Some(expected) && !state.revocation_failed {
                return Ok(RefreshResult::Published);
            }
            return Ok(RefreshResult::Unverified);
        }
        for (id, scope) in witnesses {
            match self.source.verify_persisted_fence(&scope) {
                Ok(generation) if generation == expected => {}
                Ok(generation) if generation > expected => return Ok(RefreshResult::Retry),
                _ => continue,
            };
            let Ok(guard) = self.source.guard_admission(&BTreeSet::new()) else {
                continue;
            };
            if guard.generation() != expected {
                return Ok(RefreshResult::Retry);
            }
            let mut state = self.state()?;
            if state.revocation_failed || state.generation.is_none() {
                return Ok(RefreshResult::Unverified);
            }
            if state.generation == Some(expected) {
                return Ok(RefreshResult::Published);
            }
            if state.generation != Some(from) {
                return Ok(RefreshResult::Retry);
            }
            if !state
                .fences
                .get(&id)
                .is_some_and(|fence| fence.phase == FencePhase::Persisting && fence.scope == scope)
            {
                continue;
            }
            state.generation = Some(expected);
            self.changed.notify_all();
            return Ok(RefreshResult::Published);
        }
        let state = self.state()?;
        if state.generation == Some(expected) && !state.revocation_failed {
            Ok(RefreshResult::Published)
        } else if state.generation.is_some_and(|current| current > expected)
            && !state.revocation_failed
        {
            Ok(RefreshResult::Retry)
        } else {
            Ok(RefreshResult::Unverified)
        }
    }
    /// Request cancellation of expired workers without treating expiry or a successful
    /// cancellation request as confirmation of quiescence. A host should call this on its
    /// bounded tick; it performs all host I/O outside the state lock.
    pub fn cancel_expired(&self) -> Result<(), GateError> {
        let now = self.clock.now_ns()?;
        let workers = {
            let mut s = self.state()?;
            s.live
                .iter_mut()
                .filter_map(|(id, p)| {
                    if now >= p.deadline_mono_ns && !p.cancellation_requested {
                        p.cancellation_requested = true;
                        Some((id.clone(), p.worker.clone()))
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
        };
        let mut failed = false;
        for (id, worker) in workers {
            if self.source.request_cancellation(&id, &worker).is_err() {
                failed = true;
                // A later tick may retry a failed cancellation; it is not completion.
                let mut s = self.state()?;
                if let Some(permit) = s.live.get_mut(&id).filter(|p| p.worker == worker) {
                    permit.cancellation_requested = false;
                }
            }
        }
        if failed {
            Err(GateError::DispatchAuthority("cancellation unconfirmed"))
        } else {
            Ok(())
        }
    }
    /// Retry failed durable fences; failed scopes remain denied throughout.
    pub fn retry_failed(&self) -> Result<(), GateError> {
        let ready = {
            let mut s = self.state()?;
            for f in s.fences.values_mut() {
                if f.phase == FencePhase::Failed {
                    f.phase = FencePhase::Pending;
                }
            }
            Self::ready_to_persist(&mut s)
        };
        self.persist_ready(ready)
    }
    /// Ask the trusted host to confirm termination of the exact recorded process instance.
    /// The verifier must compare both PID and start identity, not PID alone.
    pub fn confirm_terminated(&self, id: &PermitId) -> Result<bool, GateError> {
        let worker = {
            let s = self.state()?;
            match s.live.get(id) {
                Some(p) => p.worker.clone(),
                None => return Ok(false),
            }
        };
        if self
            .source
            .worker_terminated(&worker)?
            .is_none_or(|receipt| receipt.worker != worker)
        {
            return Ok(false);
        }
        let ready = {
            let mut s = self.state()?;
            if !s.live.get(id).is_some_and(|p| p.worker == worker) {
                return Ok(false);
            }
            s.live.remove(id);
            Self::ready_to_persist(&mut s)
        };
        self.changed.notify_all();
        self.persist_ready(ready)?;
        Ok(true)
    }
}
impl DispatchAuthority for BrokerState {
    fn admit(&self, req: &AdmitRequest) -> Result<Permit, GateError> {
        let requested = req
            .required_scopes()
            .ok_or(GateError::DispatchAuthority("invalid scope"))?;
        // Refuse a sealed broker without consulting a possibly unavailable source.
        if self.state()?.generation.is_none() {
            return Err(GateError::DispatchAuthority("authority sealed"));
        }
        // Authentication and I/O are deliberately outside the broker mutex.
        let verified = self.source.verify_admission(req)?;
        if verified.generation == 0
            || verified.record_epoch.len() != 64
            || !verified
                .record_epoch
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            || verified.resources.len() > 128
            || verified
                .resources
                .iter()
                .any(|s| s.is_empty() || s.len() > 4096 || s.chars().any(char::is_control))
            || verified.worker.pid == 0
            || verified.worker.start_identity == 0
            || verified.scope.len() > 128
            || !requested.is_subset(&verified.scope)
            || verified.scope.iter().any(|s| !s.valid())
        {
            return Err(GateError::DispatchAuthority("invalid verified admission"));
        }
        let deadline = self
            .clock
            .now_ns()?
            .checked_add(self.operation_ns)
            .ok_or(GateError::DispatchAuthority("permit deadline overflow"))?;
        let mut id = [0u8; 16];
        SystemRandom::new()
            .fill(&mut id)
            .map_err(|_| GateError::DispatchAuthority("permit randomness unavailable"))?;
        let id = PermitId(id);
        // Authentication is outside the broker mutex. A source-owned, scope-local guard
        // then fences all sanctioned mutations while we recheck its generation and publish
        // the permit. A bare cached generation is not sufficient to establish freshness.
        let generation_guard = self.source.guard_admission(&verified.scope)?;
        if generation_guard.generation() != verified.generation {
            return Err(GateError::DispatchAuthority("stale source authority"));
        }
        let mut s = self.state()?;
        if s.generation != Some(verified.generation) {
            return Err(GateError::DispatchAuthority("sealed or stale authority"));
        }
        if s.fences
            .values()
            .any(|f| !f.scope.is_disjoint(&verified.scope))
        {
            return Err(GateError::DispatchAuthority("scope revoked or pending"));
        }
        if s.live.len() >= 4096 {
            return Err(GateError::DispatchAuthority("permit capacity reached"));
        }
        if s.live.contains_key(&id) {
            return Err(GateError::DispatchAuthority("permit collision"));
        }
        if self.clock.now_ns()? >= deadline {
            return Err(GateError::DispatchAuthority("permit expired"));
        }
        s.live.insert(
            id.clone(),
            LivePermit {
                scope: verified.scope,
                worker: verified.worker,
                deadline_mono_ns: deadline,
                cancellation_requested: false,
            },
        );
        Ok(Permit {
            id,
            deadline_mono_ns: deadline,
            record_epoch: verified.record_epoch,
            resources: verified.resources,
        })
    }
    fn complete(&self, id: &PermitId) -> Result<(), GateError> {
        let ready = {
            let mut s = self.state()?;
            s.live.remove(id);
            Self::ready_to_persist(&mut s)
        };
        self.changed.notify_all();
        self.persist_ready(ready)
    }
    fn revoke(
        &self,
        scope: &BTreeSet<ScopeKey>,
        reason: &str,
        wait: Duration,
    ) -> Result<RevokeAck, GateError> {
        if wait > Duration::from_secs(30) {
            return Err(GateError::DispatchAuthority(
                "revocation wait exceeds limit",
            ));
        }
        if scope.is_empty()
            || scope.len() > 128
            || scope.iter().any(|s| !s.valid())
            || reason.trim().is_empty()
            || reason.len() > 1024
            || reason.chars().any(char::is_control)
        {
            return Err(GateError::DispatchAuthority("invalid revocation"));
        }
        let mut refresh_attempts = 0;
        let (index, ready) = loop {
            let resolved = self.source.resolve_revocation(scope);
            let (generation, expanded) = match resolved {
                Ok(pair) => pair,
                Err(error) => {
                    // If aliases cannot be authenticated, do not leave an equivalent scope open.
                    let mut state = self.state()?;
                    state.generation = None;
                    state.revocation_failed = true;
                    return Err(error);
                }
            };
            if expanded.is_empty()
                || expanded.len() > 128
                || !scope.is_subset(&expanded)
                || expanded.iter().any(|s| !s.valid())
            {
                let mut state = self.state()?;
                state.generation = None;
                state.revocation_failed = true;
                return Err(GateError::DispatchAuthority("invalid revocation aliases"));
            }
            // The resolved alias set is authenticated outside the broker mutex. Fence the
            // source before publishing the revocation so it cannot change between those steps.
            let generation_guard = match self.source.guard_admission(&expanded) {
                Ok(guard) if guard.generation() == generation => guard,
                Ok(guard) => {
                    let current = guard.generation();
                    drop(guard);
                    if current > generation && refresh_attempts < 8 {
                        refresh_attempts += 1;
                        match self.refresh_during_persistence(current)? {
                            RefreshResult::Published | RefreshResult::Retry => continue,
                            RefreshResult::Unverified => {}
                        }
                    }
                    let mut state = self.state()?;
                    state.generation = None;
                    state.revocation_failed = true;
                    return Err(GateError::DispatchAuthority(
                        "stale or uncoordinated revocation",
                    ));
                }
                Err(_) => {
                    let mut state = self.state()?;
                    state.generation = None;
                    state.revocation_failed = true;
                    return Err(GateError::DispatchAuthority(
                        "stale or uncoordinated revocation",
                    ));
                }
            };
            let (index, ready) = {
                let mut s = self.state()?;
                if s.generation != Some(generation) {
                    let may_refresh = s.generation.is_some_and(|current| generation > current)
                        && !s.revocation_failed
                        && s.fences.values().any(|f| f.phase == FencePhase::Persisting)
                        && refresh_attempts < 8;
                    if may_refresh {
                        drop(s);
                        drop(generation_guard);
                        refresh_attempts += 1;
                        match self.refresh_during_persistence(generation)? {
                            RefreshResult::Published | RefreshResult::Retry => continue,
                            RefreshResult::Unverified => {}
                        }
                        let mut state = self.state()?;
                        state.generation = None;
                        state.revocation_failed = true;
                        return Err(GateError::DispatchAuthority(
                            "unverified authority generation",
                        ));
                    }
                    s.generation = None;
                    s.revocation_failed = true;
                    return Err(GateError::DispatchAuthority("sealed or stale authority"));
                }
                // Capacity exhaustion seals all new work; it never allows a revocation to fail open.
                if s.fences.len() >= 4096 {
                    s.generation = None;
                    s.revocation_failed = true;
                    return Err(GateError::DispatchAuthority("revocation capacity reached"));
                }
                // This insertion is the revocation linearization point.
                let index = s.next_fence_id;
                let Some(next) = index.checked_add(1) else {
                    // A revocation we cannot represent must never leave authority open.
                    s.generation = None;
                    s.revocation_failed = true;
                    return Err(GateError::DispatchAuthority("revocation id exhausted"));
                };
                s.next_fence_id = next;
                s.fences.insert(
                    index,
                    Fence {
                        scope: expanded,
                        reason: reason.into(),
                        phase: FencePhase::Pending,
                        waiting: true,
                    },
                );
                let ready = Self::ready_to_persist(&mut s);
                (index, ready)
            };
            // Persistence may need the source's write side. Never hold its read fence over I/O.
            drop(generation_guard);
            break (index, ready);
        };
        if let Err(error) = self.persist_ready(ready) {
            // This caller is no longer waiting. A separately persisted fence may
            // have succeeded in the same batch; retire it if authenticated, while
            // retaining a failed/pending fence for retry and continued denial.
            let mut state = self.state()?;
            if let Some(fence) = state.fences.get_mut(&index) {
                fence.waiting = false;
                if fence.phase == FencePhase::Persisted {
                    state.fences.remove(&index);
                }
            }
            return Err(error);
        }
        let started = Instant::now();
        let mut s = self.state()?;
        loop {
            match s
                .fences
                .get(&index)
                .ok_or(GateError::DispatchAuthority("revocation fence missing"))?
                .phase
            {
                FencePhase::Persisted => {
                    s.fences.remove(&index);
                    return Ok(RevokeAck::Complete);
                }
                FencePhase::Failed => {
                    s.fences.get_mut(&index).expect("fence exists").waiting = false;
                    return Err(GateError::DispatchAuthority(
                        "revocation persistence failed",
                    ));
                }
                FencePhase::Pending | FencePhase::Persisting => {}
            }
            let remaining = wait.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                s.fences.get_mut(&index).expect("fence exists").waiting = false;
                return Ok(RevokeAck::PendingUnconfirmed);
            }
            let (next, _) = self
                .changed
                .wait_timeout(s, remaining)
                .map_err(|_| GateError::DispatchAuthority("authority poisoned"))?;
            s = next;
        }
    }
    fn health(&self) -> Result<AuthorityHealth, GateError> {
        Ok(if self.state()?.generation.is_some() {
            AuthorityHealth::Ready
        } else {
            AuthorityHealth::Sealed
        })
    }
    fn reconcile(&self) -> Result<(), GateError> {
        // Source must authenticate ledger/health outside this lock and ensure no pre-crash grant reopens.
        let generation = self.source.reconcile()?;
        if generation == 0 {
            return Err(GateError::DispatchAuthority("invalid authority generation"));
        }
        // A stale reconciliation snapshot cannot unseal the broker. Empty scope asks the
        // source to fence its complete authority state during the non-I/O recheck.
        let guard = self.source.guard_admission(&BTreeSet::new())?;
        if guard.generation() != generation {
            return Err(GateError::DispatchAuthority(
                "stale authority reconciliation",
            ));
        }
        let mut s = self.state()?;
        if s.reconciled
            || s.revocation_failed
            || !s.live.is_empty()
            || !s.fences.is_empty()
            || s.generation.is_some()
        {
            return Err(GateError::DispatchAuthority("authority cannot be reopened"));
        }
        s.generation = Some(generation);
        s.reconciled = true;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::mpsc;

    #[derive(Default)]
    struct TestClock(AtomicU64);
    impl MonotonicClock for TestClock {
        fn now_ns(&self) -> Result<u64, GateError> {
            Ok(self.0.load(Ordering::SeqCst))
        }
    }
    struct TestSource {
        generation: AtomicU64,
        generation_gate: std::sync::RwLock<()>,
        failed: AtomicBool,
        aliases_failed: AtomicBool,
        cancellation_failed: AtomicBool,
        stopped: AtomicBool,
        cancellations: AtomicU64,
        persisted: Mutex<Vec<BTreeSet<ScopeKey>>>,
        revocation_history: Mutex<BTreeMap<u64, BTreeSet<ScopeKey>>>,
    }
    impl Default for TestSource {
        fn default() -> Self {
            Self {
                generation: AtomicU64::new(1),
                generation_gate: std::sync::RwLock::new(()),
                failed: AtomicBool::new(false),
                aliases_failed: AtomicBool::new(false),
                cancellation_failed: AtomicBool::new(false),
                stopped: AtomicBool::new(false),
                cancellations: AtomicU64::new(0),
                persisted: Mutex::new(vec![]),
                revocation_history: Mutex::new(BTreeMap::new()),
            }
        }
    }
    struct TestAdmissionGuard<'a> {
        _guard: std::sync::RwLockReadGuard<'a, ()>,
        generation: u64,
    }
    impl AdmissionGenerationGuard for TestAdmissionGuard<'_> {
        fn generation(&self) -> u64 {
            self.generation
        }
    }
    impl TestSource {
        fn rotate(&self, generation: u64) {
            let _guard = self.generation_gate.write().unwrap();
            self.generation.store(generation, Ordering::SeqCst);
        }
    }
    impl AuthoritySource for TestSource {
        fn guard_admission<'a>(
            &'a self,
            _: &BTreeSet<ScopeKey>,
        ) -> Result<Box<dyn AdmissionGenerationGuard + 'a>, GateError> {
            let guard = self
                .generation_gate
                .read()
                .map_err(|_| GateError::DispatchAuthority("source gate poisoned"))?;
            Ok(Box::new(TestAdmissionGuard {
                _guard: guard,
                generation: self.generation.load(Ordering::SeqCst),
            }))
        }
        fn reconcile(&self) -> Result<u64, GateError> {
            Ok(self.generation.load(Ordering::SeqCst))
        }
        fn verify_admission(&self, req: &AdmitRequest) -> Result<VerifiedAdmission, GateError> {
            let mut scope = req
                .required_scopes()
                .ok_or(GateError::DispatchAuthority("bad request"))?;
            if req.secret == "KEY" {
                scope.insert(ScopeKey::Secret("KEY_ALIAS".into()));
            }
            if self
                .persisted
                .lock()
                .unwrap()
                .iter()
                .any(|f| !f.is_disjoint(&scope))
            {
                return Err(GateError::DispatchAuthority("durably revoked"));
            }
            Ok(VerifiedAdmission {
                generation: self.generation.load(Ordering::SeqCst),
                scope,
                record_epoch: "a".repeat(64),
                resources: vec!["repo".into()],
                worker: WorkerIdentity {
                    pid: 42,
                    start_identity: 77,
                },
            })
        }
        fn resolve_revocation(
            &self,
            scope: &BTreeSet<ScopeKey>,
        ) -> Result<(u64, BTreeSet<ScopeKey>), GateError> {
            if self.aliases_failed.load(Ordering::SeqCst) {
                return Err(GateError::DispatchAuthority("test alias failure"));
            }
            let mut expanded = scope.clone();
            if scope.contains(&ScopeKey::Secret("KEY".into()))
                || scope.contains(&ScopeKey::Secret("KEY_ALIAS".into()))
            {
                expanded.insert(ScopeKey::Secret("KEY".into()));
                expanded.insert(ScopeKey::Secret("KEY_ALIAS".into()));
            }
            Ok((self.generation.load(Ordering::SeqCst), expanded))
        }
        fn persist_fence(&self, scope: &BTreeSet<ScopeKey>, _: &str) -> Result<(), GateError> {
            if self.failed.load(Ordering::SeqCst) {
                return Err(GateError::DispatchAuthority("test persistence"));
            }
            self.persisted.lock().unwrap().push(scope.clone());
            Ok(())
        }
        fn verify_persisted_fence(&self, scope: &BTreeSet<ScopeKey>) -> Result<u64, GateError> {
            if !self
                .persisted
                .lock()
                .unwrap()
                .iter()
                .any(|f| scope.is_subset(f))
            {
                return Err(GateError::DispatchAuthority("fence not durable"));
            }
            Ok(self.generation.load(Ordering::SeqCst))
        }
        fn worker_terminated(
            &self,
            worker: &WorkerIdentity,
        ) -> Result<Option<VerifiedTermination>, GateError> {
            Ok((worker.pid == 42
                && worker.start_identity == 77
                && self.stopped.load(Ordering::SeqCst))
            .then(|| VerifiedTermination {
                worker: worker.clone(),
            }))
        }
        fn request_cancellation(&self, _: &PermitId, _: &WorkerIdentity) -> Result<(), GateError> {
            self.cancellations.fetch_add(1, Ordering::SeqCst);
            if self.cancellation_failed.load(Ordering::SeqCst) {
                Err(GateError::DispatchAuthority("test cancellation failure"))
            } else {
                Ok(())
            }
        }
    }
    fn broker() -> (Arc<BrokerState>, Arc<TestSource>, Arc<TestClock>) {
        let source = Arc::new(TestSource::default());
        let clock = Arc::new(TestClock::default());
        let broker = Arc::new(
            BrokerState::new(source.clone(), clock.clone(), Duration::from_secs(30)).unwrap(),
        );
        broker.reconcile().unwrap();
        (broker, source, clock)
    }
    fn scope(secret: &str) -> BTreeSet<ScopeKey> {
        BTreeSet::from([ScopeKey::Secret(secret.into())])
    }

    #[test]
    fn permit_id_wire_encoding_is_canonical_and_bounded() {
        let id = PermitId([0xab; 16]);
        assert_eq!(id.to_wire(), "abababababababababababababababab");
        assert_eq!(PermitId::from_wire(&id.to_wire()), Some(id));
        assert_eq!(
            PermitId::from_wire("ABABABABABABABABABABABABABABABAB"),
            None
        );
        assert_eq!(PermitId::from_wire("abab"), None);
        assert_eq!(
            PermitId::from_wire("zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz"),
            None
        );
    }

    #[test]
    fn source_without_transition_history_cannot_authorize_generation_refresh() {
        let source = TestSource::default();
        assert!(
            source
                .verify_revocation_transition(1, 2, &[scope("A")])
                .is_err()
        );
    }

    struct BlockingReconcile {
        source: TestSource,
        entered: mpsc::Sender<()>,
        resume: Mutex<mpsc::Receiver<()>>,
    }
    impl AuthoritySource for BlockingReconcile {
        fn guard_admission<'a>(
            &'a self,
            scope: &BTreeSet<ScopeKey>,
        ) -> Result<Box<dyn AdmissionGenerationGuard + 'a>, GateError> {
            self.source.guard_admission(scope)
        }
        fn reconcile(&self) -> Result<u64, GateError> {
            let generation = self.source.reconcile()?;
            self.entered.send(()).unwrap();
            self.resume.lock().unwrap().recv().unwrap();
            Ok(generation)
        }
        fn verify_admission(&self, r: &AdmitRequest) -> Result<VerifiedAdmission, GateError> {
            self.source.verify_admission(r)
        }
        fn resolve_revocation(
            &self,
            s: &BTreeSet<ScopeKey>,
        ) -> Result<(u64, BTreeSet<ScopeKey>), GateError> {
            self.source.resolve_revocation(s)
        }
        fn persist_fence(&self, s: &BTreeSet<ScopeKey>, r: &str) -> Result<(), GateError> {
            self.source.persist_fence(s, r)
        }
        fn worker_terminated(
            &self,
            w: &WorkerIdentity,
        ) -> Result<Option<VerifiedTermination>, GateError> {
            self.source.worker_terminated(w)
        }
        fn request_cancellation(&self, id: &PermitId, w: &WorkerIdentity) -> Result<(), GateError> {
            self.source.request_cancellation(id, w)
        }
    }

    #[test]
    fn failed_revoke_during_initial_reconcile_never_unseals() {
        let (entered_tx, entered_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        let source = Arc::new(BlockingReconcile {
            source: TestSource::default(),
            entered: entered_tx,
            resume: Mutex::new(resume_rx),
        });
        source.source.aliases_failed.store(true, Ordering::SeqCst);
        let broker = Arc::new(
            BrokerState::new(
                source,
                Arc::new(TestClock::default()),
                Duration::from_secs(30),
            )
            .unwrap(),
        );
        let worker = {
            let broker = broker.clone();
            std::thread::spawn(move || broker.reconcile())
        };
        entered_rx.recv().unwrap();
        assert!(
            broker
                .revoke(&scope("KEY"), "owner", Duration::ZERO)
                .is_err()
        );
        resume_tx.send(()).unwrap();
        assert!(worker.join().unwrap().is_err());
        assert_eq!(broker.health().unwrap(), AuthorityHealth::Sealed);
        assert!(broker.admit(&AdmitRequest::secret("KEY")).is_err());
    }

    #[test]
    fn revoke_while_initial_reconcile_is_unsealed_never_gets_lost() {
        let (entered_tx, entered_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        let source = Arc::new(BlockingReconcile {
            source: TestSource::default(),
            entered: entered_tx,
            resume: Mutex::new(resume_rx),
        });
        let broker = Arc::new(
            BrokerState::new(
                source,
                Arc::new(TestClock::default()),
                Duration::from_secs(30),
            )
            .unwrap(),
        );
        let worker = {
            let broker = broker.clone();
            std::thread::spawn(move || broker.reconcile())
        };
        entered_rx.recv().unwrap();
        assert!(
            broker
                .revoke(&scope("KEY"), "owner", Duration::ZERO)
                .is_err()
        );
        resume_tx.send(()).unwrap();
        assert!(
            worker.join().unwrap().is_err(),
            "failed revoke must remain latched"
        );
        assert_eq!(broker.health().unwrap(), AuthorityHealth::Sealed);
    }

    #[test]
    fn unavailable_authority_is_sealed_and_refuses_new_work() {
        let broker = BrokerState::unavailable();
        assert_eq!(broker.health().unwrap(), AuthorityHealth::Sealed);
        assert!(broker.admit(&AdmitRequest::secret("KEY")).is_err());
        assert!(broker.reconcile().is_err());
    }
    #[test]
    fn revoke_before_admission_blocks_aliases_but_not_unrelated_scope() {
        let (b, source, _) = broker();
        assert_eq!(
            b.revoke(&scope("KEY_ALIAS"), "owner", Duration::ZERO)
                .unwrap(),
            RevokeAck::Complete
        );
        assert!(b.admit(&AdmitRequest::secret("KEY")).is_err());
        assert!(b.admit(&AdmitRequest::secret("KEY_ALIAS")).is_err());
        assert!(b.admit(&AdmitRequest::secret("OTHER")).is_ok());
        assert_eq!(source.persisted.lock().unwrap().len(), 1);
    }
    #[test]
    fn admission_before_revoke_stays_unconfirmed_until_completion() {
        let (b, source, clock) = broker();
        let permit = b.admit(&AdmitRequest::secret("KEY")).unwrap();
        assert_eq!(permit.deadline_mono_ns, 30_000_000_000);
        assert_eq!(
            b.revoke(&scope("KEY"), "owner", Duration::ZERO).unwrap(),
            RevokeAck::PendingUnconfirmed
        );
        assert!(b.admit(&AdmitRequest::secret("KEY_ALIAS")).is_err());
        let unrelated = b.admit(&AdmitRequest::secret("OTHER")).unwrap();
        b.complete(&unrelated.id).unwrap();
        clock.0.store(30_000_000_000, Ordering::SeqCst);
        assert!(permit.remaining(clock.as_ref()).is_err());
        b.cancel_expired().unwrap();
        assert_eq!(source.cancellations.load(Ordering::SeqCst), 1);
        assert!(
            source.persisted.lock().unwrap().is_empty(),
            "expiry is not completion"
        );
        b.complete(&permit.id).unwrap();
        b.complete(&permit.id).unwrap();
        assert_eq!(source.persisted.lock().unwrap().len(), 1);
        assert!(b.admit(&AdmitRequest::secret("KEY")).is_err());
    }
    #[test]
    fn failed_fence_persistence_stays_denied_and_can_retry() {
        let (b, source, _) = broker();
        source.failed.store(true, Ordering::SeqCst);
        assert!(b.revoke(&scope("KEY"), "owner", Duration::ZERO).is_err());
        assert!(b.admit(&AdmitRequest::secret("KEY")).is_err());
        assert!(b.admit(&AdmitRequest::secret("OTHER")).is_ok());
        source.failed.store(false, Ordering::SeqCst);
        b.retry_failed().unwrap();
        assert_eq!(source.persisted.lock().unwrap().len(), 1);
        assert!(b.admit(&AdmitRequest::secret("KEY")).is_err());
    }
    #[test]
    fn exact_worker_termination_can_confirm_quiescence() {
        let (b, source, _) = broker();
        let permit = b.admit(&AdmitRequest::secret("KEY")).unwrap();
        assert_eq!(
            b.revoke(&scope("KEY"), "owner", Duration::ZERO).unwrap(),
            RevokeAck::PendingUnconfirmed
        );
        assert!(!b.confirm_terminated(&permit.id).unwrap());
        source.stopped.store(true, Ordering::SeqCst);
        assert!(b.confirm_terminated(&permit.id).unwrap());
        assert_eq!(source.persisted.lock().unwrap().len(), 1);
    }
    /// Authenticates data but does not provide a coordinated mutation fence.
    /// The default trait method must make this source unusable for admission.
    struct UnguardedSource(TestSource);
    impl AuthoritySource for UnguardedSource {
        fn reconcile(&self) -> Result<u64, GateError> {
            self.0.reconcile()
        }
        fn verify_admission(&self, request: &AdmitRequest) -> Result<VerifiedAdmission, GateError> {
            self.0.verify_admission(request)
        }
        fn resolve_revocation(
            &self,
            scope: &BTreeSet<ScopeKey>,
        ) -> Result<(u64, BTreeSet<ScopeKey>), GateError> {
            self.0.resolve_revocation(scope)
        }
        fn persist_fence(&self, scope: &BTreeSet<ScopeKey>, reason: &str) -> Result<(), GateError> {
            self.0.persist_fence(scope, reason)
        }
        fn verify_persisted_fence(&self, scope: &BTreeSet<ScopeKey>) -> Result<u64, GateError> {
            self.0.verify_persisted_fence(scope)
        }
        fn worker_terminated(
            &self,
            worker: &WorkerIdentity,
        ) -> Result<Option<VerifiedTermination>, GateError> {
            self.0.worker_terminated(worker)
        }
        fn request_cancellation(
            &self,
            id: &PermitId,
            worker: &WorkerIdentity,
        ) -> Result<(), GateError> {
            self.0.request_cancellation(id, worker)
        }
    }

    #[test]
    fn source_without_mutation_fence_never_admits() {
        let source = Arc::new(UnguardedSource(TestSource::default()));
        let broker = BrokerState::new(
            source,
            Arc::new(TestClock::default()),
            Duration::from_secs(30),
        )
        .unwrap();
        assert!(matches!(
            broker.reconcile(),
            Err(GateError::DispatchAuthority(
                "coordinated source unavailable"
            ))
        ));
        assert_eq!(broker.health().unwrap(), AuthorityHealth::Sealed);
        assert!(broker.admit(&AdmitRequest::secret("KEY")).is_err());
        assert!(broker.state().unwrap().live.is_empty());
    }

    struct GuardObservingClock {
        source: Arc<TestSource>,
        calls: AtomicU64,
    }
    impl MonotonicClock for GuardObservingClock {
        fn now_ns(&self) -> Result<u64, GateError> {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 1 {
                // The second clock read is inside the permit insertion section.
                // A source writer must be unable to cross its gate at that point.
                assert!(self.source.generation_gate.try_write().is_err());
            }
            Ok(0)
        }
    }

    #[test]
    fn source_mutations_remain_fenced_until_permit_is_registered() {
        let source = Arc::new(TestSource::default());
        let clock = Arc::new(GuardObservingClock {
            source: source.clone(),
            calls: AtomicU64::new(0),
        });
        let broker = BrokerState::new(source.clone(), clock, Duration::from_secs(30)).unwrap();
        broker.reconcile().unwrap();
        let permit = broker.admit(&AdmitRequest::secret("KEY")).unwrap();
        assert!(broker.state().unwrap().live.contains_key(&permit.id));
        // The writer gate is released after registration.
        assert!(source.generation_gate.try_write().is_ok());
    }

    struct BlockingAdmission {
        source: TestSource,
        verified: mpsc::Sender<()>,
        resume: Mutex<mpsc::Receiver<()>>,
    }
    impl AuthoritySource for BlockingAdmission {
        fn guard_admission<'a>(
            &'a self,
            scope: &BTreeSet<ScopeKey>,
        ) -> Result<Box<dyn AdmissionGenerationGuard + 'a>, GateError> {
            self.source.guard_admission(scope)
        }
        fn reconcile(&self) -> Result<u64, GateError> {
            self.source.reconcile()
        }
        fn verify_admission(&self, request: &AdmitRequest) -> Result<VerifiedAdmission, GateError> {
            let verified = self.source.verify_admission(request)?;
            self.verified.send(()).unwrap();
            self.resume.lock().unwrap().recv().unwrap();
            Ok(verified)
        }
        fn resolve_revocation(
            &self,
            scope: &BTreeSet<ScopeKey>,
        ) -> Result<(u64, BTreeSet<ScopeKey>), GateError> {
            self.source.resolve_revocation(scope)
        }
        fn persist_fence(&self, scope: &BTreeSet<ScopeKey>, reason: &str) -> Result<(), GateError> {
            self.source.persist_fence(scope, reason)
        }
        fn verify_persisted_fence(&self, scope: &BTreeSet<ScopeKey>) -> Result<u64, GateError> {
            self.source.verify_persisted_fence(scope)
        }
        fn worker_terminated(
            &self,
            worker: &WorkerIdentity,
        ) -> Result<Option<VerifiedTermination>, GateError> {
            self.source.worker_terminated(worker)
        }
        fn request_cancellation(
            &self,
            id: &PermitId,
            worker: &WorkerIdentity,
        ) -> Result<(), GateError> {
            self.source.request_cancellation(id, worker)
        }
    }

    #[test]
    fn rotated_source_after_verification_never_grants_stale_permit() {
        let (verified_tx, verified_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        let source = Arc::new(BlockingAdmission {
            source: TestSource::default(),
            verified: verified_tx,
            resume: Mutex::new(resume_rx),
        });
        let broker = Arc::new(
            BrokerState::new(
                source.clone(),
                Arc::new(TestClock::default()),
                Duration::from_secs(30),
            )
            .unwrap(),
        );
        broker.reconcile().unwrap();
        let worker = {
            let broker = broker.clone();
            std::thread::spawn(move || broker.admit(&AdmitRequest::secret("KEY")))
        };
        verified_rx.recv().unwrap();
        source.source.rotate(2);
        resume_tx.send(()).unwrap();
        assert!(
            worker.join().unwrap().is_err(),
            "old authenticated generation must not authorize after rotation"
        );
    }

    #[test]
    fn stale_verified_generation_does_not_grant() {
        let (b, source, _) = broker();
        source.rotate(2);
        assert!(b.admit(&AdmitRequest::secret("KEY")).is_err());
        assert!(b.revoke(&scope("KEY"), "owner", Duration::ZERO).is_err());
    }
    #[test]
    fn bundle_admission_requires_both_bundle_and_incarnation_scopes() {
        let (b, _, _) = broker();
        assert!(
            b.admit(&AdmitRequest {
                secret: "KEY".into(),
                bundle: Some("b".into()),
                incarnation: None
            })
            .is_err()
        );
        let req = AdmitRequest {
            secret: "KEY".into(),
            bundle: Some("b".into()),
            incarnation: Some("i".into()),
        };
        let permit = b.admit(&req).unwrap();
        assert_eq!(
            b.revoke(
                &BTreeSet::from([ScopeKey::Incarnation("i".into())]),
                "owner",
                Duration::ZERO
            )
            .unwrap(),
            RevokeAck::PendingUnconfirmed
        );
        assert!(b.admit(&req).is_err());
        b.complete(&permit.id).unwrap();
    }
    #[test]
    fn unresolvable_revocation_seals_new_work() {
        let (broker, source, _) = broker();
        source.aliases_failed.store(true, Ordering::SeqCst);
        assert!(
            broker
                .revoke(&scope("KEY"), "owner", Duration::ZERO)
                .is_err()
        );
        source.aliases_failed.store(false, Ordering::SeqCst);
        assert_eq!(broker.health().unwrap(), AuthorityHealth::Sealed);
        assert!(
            broker.reconcile().is_err(),
            "failed revocation cannot be reopened"
        );
        assert!(broker.admit(&AdmitRequest::secret("KEY")).is_err());
        assert!(broker.admit(&AdmitRequest::secret("OTHER")).is_err());
    }
    #[test]
    fn cancellation_failure_keeps_permit_and_can_be_retried() {
        let (broker, source, clock) = broker();
        let permit = broker.admit(&AdmitRequest::secret("KEY")).unwrap();
        assert_eq!(
            broker
                .revoke(&scope("KEY"), "owner", Duration::ZERO)
                .unwrap(),
            RevokeAck::PendingUnconfirmed
        );
        clock.0.store(30_000_000_000, Ordering::SeqCst);
        source.cancellation_failed.store(true, Ordering::SeqCst);
        assert!(broker.cancel_expired().is_err());
        assert!(source.persisted.lock().unwrap().is_empty());
        source.cancellation_failed.store(false, Ordering::SeqCst);
        broker.cancel_expired().unwrap();
        assert_eq!(source.cancellations.load(Ordering::SeqCst), 2);
        assert!(source.persisted.lock().unwrap().is_empty());
        broker.complete(&permit.id).unwrap();
        assert_eq!(source.persisted.lock().unwrap().len(), 1);
    }
    #[test]
    fn drain_wait_releases_mutex_and_unrelated_traffic_proceeds() {
        let (b, _, _) = broker();
        let permit = b.admit(&AdmitRequest::secret("KEY")).unwrap();
        let (sent, received) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        let clone = b.clone();
        let handle = std::thread::spawn(move || {
            ready_tx.send(()).unwrap();
            sent.send(clone.revoke(&scope("KEY"), "owner", Duration::from_secs(2)))
                .unwrap();
        });
        ready_rx.recv().unwrap();
        // Wait until revocation's linearization point using observable state, not a sleep.
        let started = Instant::now();
        while !b
            .state()
            .unwrap()
            .fences
            .values()
            .any(|f| f.scope.contains(&ScopeKey::Secret("KEY".into())))
        {
            assert!(started.elapsed() < Duration::from_secs(2));
            std::thread::yield_now();
        }
        assert!(b.admit(&AdmitRequest::secret("OTHER")).is_ok());
        assert!(b.admit(&AdmitRequest::secret("KEY")).is_err());
        b.complete(&permit.id).unwrap();
        assert_eq!(
            received
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .unwrap(),
            RevokeAck::Complete
        );
        handle.join().unwrap();
    }
    /// A source whose durable revocations change its authenticated generation and are
    /// enforced during subsequent verification, as a production source must do.
    struct AdvancingSource(TestSource);
    impl AuthoritySource for AdvancingSource {
        fn reconcile(&self) -> Result<u64, GateError> {
            self.0.reconcile()
        }
        fn verify_admission(&self, req: &AdmitRequest) -> Result<VerifiedAdmission, GateError> {
            let requested = req
                .required_scopes()
                .ok_or(GateError::DispatchAuthority("bad request"))?;
            if self
                .0
                .persisted
                .lock()
                .unwrap()
                .iter()
                .any(|s| !s.is_disjoint(&requested))
            {
                return Err(GateError::DispatchAuthority("durably revoked"));
            }
            self.0.verify_admission(req)
        }
        fn guard_admission<'a>(
            &'a self,
            scope: &BTreeSet<ScopeKey>,
        ) -> Result<Box<dyn AdmissionGenerationGuard + 'a>, GateError> {
            self.0.guard_admission(scope)
        }
        fn resolve_revocation(
            &self,
            scope: &BTreeSet<ScopeKey>,
        ) -> Result<(u64, BTreeSet<ScopeKey>), GateError> {
            self.0.resolve_revocation(scope)
        }
        fn persist_fence(&self, scope: &BTreeSet<ScopeKey>, _: &str) -> Result<(), GateError> {
            let _guard = self.0.generation_gate.write().unwrap();
            self.0.persisted.lock().unwrap().push(scope.clone());
            let generation = self.0.generation.fetch_add(1, Ordering::SeqCst) + 1;
            self.0
                .revocation_history
                .lock()
                .unwrap()
                .insert(generation, scope.clone());
            Ok(())
        }
        fn verify_persisted_fence(&self, scope: &BTreeSet<ScopeKey>) -> Result<u64, GateError> {
            self.0.verify_persisted_fence(scope)
        }
        fn verify_revocation_transition(
            &self,
            from: u64,
            to: u64,
            pending: &[BTreeSet<ScopeKey>],
        ) -> Result<(), GateError> {
            let _guard = self.0.generation_gate.read().unwrap();
            let history = self.0.revocation_history.lock().unwrap();
            let start = from
                .checked_add(1)
                .ok_or(GateError::DispatchAuthority("generation overflow"))?;
            if to != self.0.generation.load(Ordering::SeqCst)
                || start > to
                || to.saturating_sub(from) > 4096
                || (start..=to).any(|generation| {
                    history
                        .get(&generation)
                        .is_none_or(|scope| !pending.contains(scope))
                })
            {
                return Err(GateError::DispatchAuthority("unverified transition"));
            }
            Ok(())
        }
        fn worker_terminated(
            &self,
            worker: &WorkerIdentity,
        ) -> Result<Option<VerifiedTermination>, GateError> {
            self.0.worker_terminated(worker)
        }
        fn request_cancellation(
            &self,
            id: &PermitId,
            worker: &WorkerIdentity,
        ) -> Result<(), GateError> {
            self.0.request_cancellation(id, worker)
        }
    }
    #[test]
    fn persisted_fence_generation_does_not_block_unrelated_admissions() {
        let source = Arc::new(AdvancingSource(TestSource::default()));
        let broker = BrokerState::new(
            source,
            Arc::new(TestClock::default()),
            Duration::from_secs(30),
        )
        .unwrap();
        broker.reconcile().unwrap();
        assert_eq!(
            broker
                .revoke(&scope("KEY"), "owner", Duration::ZERO)
                .unwrap(),
            RevokeAck::Complete
        );
        assert!(broker.admit(&AdmitRequest::secret("KEY")).is_err());
        assert!(broker.admit(&AdmitRequest::secret("OTHER")).is_ok());
    }

    /// Pause one revocation after its durable write advanced the source generation.
    struct PausedFenceVerification {
        source: AdvancingSource,
        entered: mpsc::Sender<()>,
        resume: Mutex<mpsc::Receiver<()>>,
        paused: AtomicBool,
    }
    impl AuthoritySource for PausedFenceVerification {
        fn reconcile(&self) -> Result<u64, GateError> {
            self.source.reconcile()
        }
        fn verify_admission(&self, req: &AdmitRequest) -> Result<VerifiedAdmission, GateError> {
            self.source.verify_admission(req)
        }
        fn guard_admission<'a>(
            &'a self,
            scope: &BTreeSet<ScopeKey>,
        ) -> Result<Box<dyn AdmissionGenerationGuard + 'a>, GateError> {
            self.source.guard_admission(scope)
        }
        fn resolve_revocation(
            &self,
            scope: &BTreeSet<ScopeKey>,
        ) -> Result<(u64, BTreeSet<ScopeKey>), GateError> {
            self.source.resolve_revocation(scope)
        }
        fn persist_fence(&self, scope: &BTreeSet<ScopeKey>, reason: &str) -> Result<(), GateError> {
            self.source.persist_fence(scope, reason)
        }
        fn verify_persisted_fence(&self, scope: &BTreeSet<ScopeKey>) -> Result<u64, GateError> {
            if scope.contains(&ScopeKey::Secret("A".into()))
                && !self.paused.swap(true, Ordering::SeqCst)
            {
                self.entered.send(()).unwrap();
                self.resume.lock().unwrap().recv().unwrap();
            }
            self.source.verify_persisted_fence(scope)
        }
        fn verify_revocation_transition(
            &self,
            from: u64,
            to: u64,
            pending: &[BTreeSet<ScopeKey>],
        ) -> Result<(), GateError> {
            self.source.verify_revocation_transition(from, to, pending)
        }
        fn worker_terminated(
            &self,
            worker: &WorkerIdentity,
        ) -> Result<Option<VerifiedTermination>, GateError> {
            self.source.worker_terminated(worker)
        }
        fn request_cancellation(
            &self,
            id: &PermitId,
            worker: &WorkerIdentity,
        ) -> Result<(), GateError> {
            self.source.request_cancellation(id, worker)
        }
    }

    #[test]
    fn unrelated_revoke_during_generation_publication_keeps_both_fences() {
        let (entered_tx, entered_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        let source = Arc::new(PausedFenceVerification {
            source: AdvancingSource(TestSource::default()),
            entered: entered_tx,
            resume: Mutex::new(resume_rx),
            paused: AtomicBool::new(false),
        });
        let broker = Arc::new(
            BrokerState::new(
                source,
                Arc::new(TestClock::default()),
                Duration::from_secs(30),
            )
            .unwrap(),
        );
        broker.reconcile().unwrap();
        let worker = {
            let broker = broker.clone();
            std::thread::spawn(move || broker.revoke(&scope("A"), "owner", Duration::from_secs(2)))
        };
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let b = broker.revoke(&scope("B"), "owner", Duration::from_secs(2));
        assert_eq!(b.unwrap(), RevokeAck::Complete);
        // A's first verifier is still paused. Its in-memory fence must continue to
        // deny A while the now-authenticated generation permits unrelated work.
        assert!(broker.admit(&AdmitRequest::secret("A")).is_err());
        assert!(broker.admit(&AdmitRequest::secret("B")).is_err());
        assert!(broker.admit(&AdmitRequest::secret("OTHER")).is_ok());
        resume_tx.send(()).unwrap();
        assert_eq!(worker.join().unwrap().unwrap(), RevokeAck::Complete);
        assert!(broker.admit(&AdmitRequest::secret("A")).is_err());
        assert!(broker.admit(&AdmitRequest::secret("B")).is_err());
        assert!(broker.admit(&AdmitRequest::secret("OTHER")).is_ok());
    }

    struct PausedAliasResolution {
        source: AdvancingSource,
        entered: mpsc::Sender<()>,
        resume: Mutex<mpsc::Receiver<()>>,
        paused: AtomicBool,
    }
    impl AuthoritySource for PausedAliasResolution {
        fn reconcile(&self) -> Result<u64, GateError> {
            self.source.reconcile()
        }
        fn verify_admission(&self, req: &AdmitRequest) -> Result<VerifiedAdmission, GateError> {
            self.source.verify_admission(req)
        }
        fn guard_admission<'a>(
            &'a self,
            scope: &BTreeSet<ScopeKey>,
        ) -> Result<Box<dyn AdmissionGenerationGuard + 'a>, GateError> {
            self.source.guard_admission(scope)
        }
        fn resolve_revocation(
            &self,
            scope: &BTreeSet<ScopeKey>,
        ) -> Result<(u64, BTreeSet<ScopeKey>), GateError> {
            let resolved = self.source.resolve_revocation(scope)?;
            if scope.contains(&ScopeKey::Secret("B".into()))
                && !self.paused.swap(true, Ordering::SeqCst)
            {
                self.entered.send(()).unwrap();
                self.resume.lock().unwrap().recv().unwrap();
            }
            Ok(resolved)
        }
        fn persist_fence(&self, scope: &BTreeSet<ScopeKey>, reason: &str) -> Result<(), GateError> {
            self.source.persist_fence(scope, reason)
        }
        fn verify_persisted_fence(&self, scope: &BTreeSet<ScopeKey>) -> Result<u64, GateError> {
            self.source.verify_persisted_fence(scope)
        }
        fn verify_revocation_transition(
            &self,
            from: u64,
            to: u64,
            pending: &[BTreeSet<ScopeKey>],
        ) -> Result<(), GateError> {
            self.source.verify_revocation_transition(from, to, pending)
        }
        fn worker_terminated(
            &self,
            worker: &WorkerIdentity,
        ) -> Result<Option<VerifiedTermination>, GateError> {
            self.source.worker_terminated(worker)
        }
        fn request_cancellation(
            &self,
            id: &PermitId,
            worker: &WorkerIdentity,
        ) -> Result<(), GateError> {
            self.source.request_cancellation(id, worker)
        }
    }

    #[test]
    fn revoke_re_resolves_aliases_when_another_fence_publishes_first() {
        let (entered_tx, entered_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        let source = Arc::new(PausedAliasResolution {
            source: AdvancingSource(TestSource::default()),
            entered: entered_tx,
            resume: Mutex::new(resume_rx),
            paused: AtomicBool::new(false),
        });
        let broker = Arc::new(
            BrokerState::new(
                source,
                Arc::new(TestClock::default()),
                Duration::from_secs(30),
            )
            .unwrap(),
        );
        broker.reconcile().unwrap();
        let b = {
            let broker = broker.clone();
            std::thread::spawn(move || broker.revoke(&scope("B"), "owner", Duration::from_secs(2)))
        };
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(
            broker
                .revoke(&scope("A"), "owner", Duration::from_secs(2))
                .unwrap(),
            RevokeAck::Complete
        );
        resume_tx.send(()).unwrap();
        assert_eq!(b.join().unwrap().unwrap(), RevokeAck::Complete);
        assert!(broker.admit(&AdmitRequest::secret("A")).is_err());
        assert!(broker.admit(&AdmitRequest::secret("B")).is_err());
        assert!(broker.admit(&AdmitRequest::secret("OTHER")).is_ok());
    }

    #[test]
    fn unrelated_mutation_after_persisted_fence_never_publishes_generation() {
        let (entered_tx, entered_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        let source = Arc::new(PausedFenceVerification {
            source: AdvancingSource(TestSource::default()),
            entered: entered_tx,
            resume: Mutex::new(resume_rx),
            paused: AtomicBool::new(false),
        });
        let broker = Arc::new(
            BrokerState::new(
                source.clone(),
                Arc::new(TestClock::default()),
                Duration::from_secs(30),
            )
            .unwrap(),
        );
        broker.reconcile().unwrap();
        let worker = {
            let broker = broker.clone();
            std::thread::spawn(move || broker.revoke(&scope("A"), "owner", Duration::from_secs(2)))
        };
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        // A is durable at generation 2. This separate mutation advances to 3,
        // without a revocation-transition record that can authorize publication.
        source.source.0.rotate(3);
        assert!(
            broker
                .revoke(&scope("B"), "owner", Duration::from_secs(2))
                .is_err()
        );
        assert_eq!(broker.health().unwrap(), AuthorityHealth::Sealed);
        assert!(broker.admit(&AdmitRequest::secret("OTHER")).is_err());
        resume_tx.send(()).unwrap();
        assert!(worker.join().unwrap().is_err());
    }

    #[test]
    fn persisted_fence_alone_cannot_publish_unrelated_authority_mutation() {
        let (entered_tx, entered_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        let source = Arc::new(PausedFenceVerification {
            source: AdvancingSource(TestSource::default()),
            entered: entered_tx,
            resume: Mutex::new(resume_rx),
            paused: AtomicBool::new(false),
        });
        let broker = Arc::new(
            BrokerState::new(
                source.clone(),
                Arc::new(TestClock::default()),
                Duration::from_secs(30),
            )
            .unwrap(),
        );
        broker.reconcile().unwrap();
        let worker = {
            let broker = broker.clone();
            std::thread::spawn(move || broker.revoke(&scope("A"), "owner", Duration::from_secs(2)))
        };
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        source.source.0.rotate(3);
        resume_tx.send(()).unwrap();
        assert!(worker.join().unwrap().is_err());
        assert_ne!(broker.state().unwrap().generation, Some(3));
        assert!(broker.admit(&AdmitRequest::secret("OTHER")).is_err());
    }

    /// A generation advance is not evidence of a fence that has not been persisted.
    struct PausedBeforePersistence {
        source: AdvancingSource,
        entered: mpsc::Sender<()>,
        resume: Mutex<mpsc::Receiver<()>>,
    }
    impl AuthoritySource for PausedBeforePersistence {
        fn reconcile(&self) -> Result<u64, GateError> {
            self.source.reconcile()
        }
        fn verify_admission(&self, req: &AdmitRequest) -> Result<VerifiedAdmission, GateError> {
            self.source.verify_admission(req)
        }
        fn guard_admission<'a>(
            &'a self,
            scope: &BTreeSet<ScopeKey>,
        ) -> Result<Box<dyn AdmissionGenerationGuard + 'a>, GateError> {
            self.source.guard_admission(scope)
        }
        fn resolve_revocation(
            &self,
            scope: &BTreeSet<ScopeKey>,
        ) -> Result<(u64, BTreeSet<ScopeKey>), GateError> {
            self.source.resolve_revocation(scope)
        }
        fn persist_fence(&self, scope: &BTreeSet<ScopeKey>, reason: &str) -> Result<(), GateError> {
            if scope.contains(&ScopeKey::Secret("A".into())) {
                self.entered.send(()).unwrap();
                self.resume.lock().unwrap().recv().unwrap();
            }
            self.source.persist_fence(scope, reason)
        }
        fn verify_persisted_fence(&self, scope: &BTreeSet<ScopeKey>) -> Result<u64, GateError> {
            self.source.verify_persisted_fence(scope)
        }
        fn verify_revocation_transition(
            &self,
            from: u64,
            to: u64,
            pending: &[BTreeSet<ScopeKey>],
        ) -> Result<(), GateError> {
            self.source.verify_revocation_transition(from, to, pending)
        }
        fn worker_terminated(
            &self,
            worker: &WorkerIdentity,
        ) -> Result<Option<VerifiedTermination>, GateError> {
            self.source.worker_terminated(worker)
        }
        fn request_cancellation(
            &self,
            id: &PermitId,
            worker: &WorkerIdentity,
        ) -> Result<(), GateError> {
            self.source.request_cancellation(id, worker)
        }
    }

    #[test]
    fn unrelated_generation_advance_without_durable_witness_seals() {
        let (entered_tx, entered_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        let source = Arc::new(PausedBeforePersistence {
            source: AdvancingSource(TestSource::default()),
            entered: entered_tx,
            resume: Mutex::new(resume_rx),
        });
        let broker = Arc::new(
            BrokerState::new(
                source.clone(),
                Arc::new(TestClock::default()),
                Duration::from_secs(30),
            )
            .unwrap(),
        );
        broker.reconcile().unwrap();
        let worker = {
            let broker = broker.clone();
            std::thread::spawn(move || broker.revoke(&scope("A"), "owner", Duration::from_secs(2)))
        };
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        source.source.0.rotate(2);
        assert!(
            broker
                .revoke(&scope("B"), "owner", Duration::from_secs(2))
                .is_err()
        );
        assert_eq!(broker.health().unwrap(), AuthorityHealth::Sealed);
        assert!(broker.admit(&AdmitRequest::secret("OTHER")).is_err());
        resume_tx.send(()).unwrap();
        assert!(worker.join().unwrap().is_err());
    }
    #[test]
    fn durable_fences_do_not_exhaust_pending_fence_capacity() {
        let source = Arc::new(AdvancingSource(TestSource::default()));
        let broker = BrokerState::new(
            source,
            Arc::new(TestClock::default()),
            Duration::from_secs(30),
        )
        .unwrap();
        broker.reconcile().unwrap();
        for i in 0..4100 {
            assert_eq!(
                broker
                    .revoke(&scope(&format!("KEY{i}")), "owner", Duration::ZERO)
                    .unwrap(),
                RevokeAck::Complete,
                "{i}"
            );
        }
        assert!(broker.admit(&AdmitRequest::secret("KEY0")).is_err());
        assert!(broker.admit(&AdmitRequest::secret("OTHER")).is_ok());
    }

    /// A source may persist a fence yet fail to authenticate the resulting snapshot.
    /// The broker must keep denying it rather than retiring it on an unverified write.
    struct UnverifiedFenceSource(TestSource);
    impl AuthoritySource for UnverifiedFenceSource {
        fn reconcile(&self) -> Result<u64, GateError> {
            self.0.reconcile()
        }
        fn verify_admission(&self, r: &AdmitRequest) -> Result<VerifiedAdmission, GateError> {
            self.0.verify_admission(r)
        }
        fn guard_admission<'a>(
            &'a self,
            s: &BTreeSet<ScopeKey>,
        ) -> Result<Box<dyn AdmissionGenerationGuard + 'a>, GateError> {
            self.0.guard_admission(s)
        }
        fn resolve_revocation(
            &self,
            s: &BTreeSet<ScopeKey>,
        ) -> Result<(u64, BTreeSet<ScopeKey>), GateError> {
            self.0.resolve_revocation(s)
        }
        fn persist_fence(&self, s: &BTreeSet<ScopeKey>, r: &str) -> Result<(), GateError> {
            self.0.persist_fence(s, r)
        }
        // Deliberately use the trait's fail-closed default: no authenticated proof of persistence.
        fn worker_terminated(
            &self,
            w: &WorkerIdentity,
        ) -> Result<Option<VerifiedTermination>, GateError> {
            self.0.worker_terminated(w)
        }
        fn request_cancellation(&self, id: &PermitId, w: &WorkerIdentity) -> Result<(), GateError> {
            self.0.request_cancellation(id, w)
        }
    }
    #[test]
    fn unverified_durable_fence_is_not_retired() {
        let source = Arc::new(UnverifiedFenceSource(TestSource::default()));
        let broker = BrokerState::new(
            source,
            Arc::new(TestClock::default()),
            Duration::from_secs(30),
        )
        .unwrap();
        broker.reconcile().unwrap();
        assert!(
            broker
                .revoke(&scope("KEY"), "owner", Duration::ZERO)
                .is_err()
        );
        let s = broker.state().unwrap();
        assert_eq!(s.fences.len(), 1);
        assert_eq!(s.fences.values().next().unwrap().phase, FencePhase::Failed);
        drop(s);
        assert!(broker.admit(&AdmitRequest::secret("KEY")).is_err());
        assert!(broker.admit(&AdmitRequest::secret("OTHER")).is_ok());
    }
    #[test]
    fn timed_out_fence_retires_only_after_its_permit_completes() {
        let source = Arc::new(AdvancingSource(TestSource::default()));
        let broker = BrokerState::new(
            source,
            Arc::new(TestClock::default()),
            Duration::from_secs(30),
        )
        .unwrap();
        broker.reconcile().unwrap();
        let permit = broker.admit(&AdmitRequest::secret("KEY")).unwrap();
        assert_eq!(
            broker
                .revoke(&scope("KEY"), "owner", Duration::ZERO)
                .unwrap(),
            RevokeAck::PendingUnconfirmed
        );
        assert_eq!(broker.state().unwrap().fences.len(), 1);
        broker.complete(&permit.id).unwrap();
        assert!(broker.state().unwrap().fences.is_empty());
        assert!(broker.admit(&AdmitRequest::secret("KEY")).is_err());
        assert!(broker.admit(&AdmitRequest::secret("OTHER")).is_ok());
    }
    #[test]
    fn concurrent_draining_fences_use_stable_ids() {
        let source = Arc::new(AdvancingSource(TestSource::default()));
        let broker = Arc::new(
            BrokerState::new(
                source,
                Arc::new(TestClock::default()),
                Duration::from_secs(30),
            )
            .unwrap(),
        );
        broker.reconcile().unwrap();
        let a = broker.admit(&AdmitRequest::secret("A")).unwrap();
        let b = broker.admit(&AdmitRequest::secret("B")).unwrap();
        assert_eq!(
            broker.revoke(&scope("A"), "owner", Duration::ZERO).unwrap(),
            RevokeAck::PendingUnconfirmed
        );
        assert_eq!(
            broker.revoke(&scope("B"), "owner", Duration::ZERO).unwrap(),
            RevokeAck::PendingUnconfirmed
        );
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let handles = [a.id, b.id]
            .into_iter()
            .map(|id| {
                let broker = broker.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    broker.complete(&id)
                })
            })
            .collect::<Vec<_>>();
        barrier.wait();
        for h in handles {
            h.join().unwrap().unwrap();
        }
        assert!(broker.state().unwrap().fences.is_empty());
        assert!(broker.admit(&AdmitRequest::secret("A")).is_err());
        assert!(broker.admit(&AdmitRequest::secret("B")).is_err());
        assert!(broker.admit(&AdmitRequest::secret("C")).is_ok());
    }
    struct RacingFenceSource {
        source: AdvancingSource,
        entered: mpsc::Sender<()>,
        resume: Mutex<mpsc::Receiver<()>>,
        once: AtomicBool,
    }
    impl AuthoritySource for RacingFenceSource {
        fn reconcile(&self) -> Result<u64, GateError> {
            self.source.reconcile()
        }
        fn verify_admission(&self, r: &AdmitRequest) -> Result<VerifiedAdmission, GateError> {
            self.source.verify_admission(r)
        }
        fn guard_admission<'a>(
            &'a self,
            s: &BTreeSet<ScopeKey>,
        ) -> Result<Box<dyn AdmissionGenerationGuard + 'a>, GateError> {
            self.source.guard_admission(s)
        }
        fn resolve_revocation(
            &self,
            s: &BTreeSet<ScopeKey>,
        ) -> Result<(u64, BTreeSet<ScopeKey>), GateError> {
            self.source.resolve_revocation(s)
        }
        fn persist_fence(&self, s: &BTreeSet<ScopeKey>, r: &str) -> Result<(), GateError> {
            self.source.persist_fence(s, r)
        }
        fn verify_persisted_fence(&self, s: &BTreeSet<ScopeKey>) -> Result<u64, GateError> {
            let generation = self.source.verify_persisted_fence(s)?;
            if s.contains(&ScopeKey::Secret("A".into())) && !self.once.swap(true, Ordering::SeqCst)
            {
                self.entered.send(()).unwrap();
                self.resume.lock().unwrap().recv().unwrap();
            }
            Ok(generation)
        }
        fn verify_revocation_transition(
            &self,
            from: u64,
            to: u64,
            pending: &[BTreeSet<ScopeKey>],
        ) -> Result<(), GateError> {
            self.source.verify_revocation_transition(from, to, pending)
        }
        fn worker_terminated(
            &self,
            w: &WorkerIdentity,
        ) -> Result<Option<VerifiedTermination>, GateError> {
            self.source.worker_terminated(w)
        }
        fn request_cancellation(&self, id: &PermitId, w: &WorkerIdentity) -> Result<(), GateError> {
            self.source.request_cancellation(id, w)
        }
    }
    #[test]
    fn persisted_fence_reauthenticates_after_concurrent_generation_change() {
        let (entered_tx, entered_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        let source = Arc::new(RacingFenceSource {
            source: AdvancingSource(TestSource::default()),
            entered: entered_tx,
            resume: Mutex::new(resume_rx),
            once: AtomicBool::new(false),
        });
        let broker = Arc::new(
            BrokerState::new(
                source,
                Arc::new(TestClock::default()),
                Duration::from_secs(30),
            )
            .unwrap(),
        );
        broker.reconcile().unwrap();
        let a = broker.admit(&AdmitRequest::secret("A")).unwrap();
        let b = broker.admit(&AdmitRequest::secret("B")).unwrap();
        assert_eq!(
            broker.revoke(&scope("A"), "owner", Duration::ZERO).unwrap(),
            RevokeAck::PendingUnconfirmed
        );
        assert_eq!(
            broker.revoke(&scope("B"), "owner", Duration::ZERO).unwrap(),
            RevokeAck::PendingUnconfirmed
        );
        let worker = {
            let broker = broker.clone();
            std::thread::spawn(move || broker.complete(&a.id))
        };
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        broker.complete(&b.id).unwrap();
        resume_tx.send(()).unwrap();
        worker.join().unwrap().unwrap();
        assert!(broker.state().unwrap().fences.is_empty());
        assert!(broker.admit(&AdmitRequest::secret("A")).is_err());
        assert!(broker.admit(&AdmitRequest::secret("B")).is_err());
        assert!(broker.admit(&AdmitRequest::secret("C")).is_ok());
    }
    #[test]
    fn exhausted_fence_identity_seals_instead_of_leaving_revocation_open() {
        let (broker, _, _) = broker();
        broker.state().unwrap().next_fence_id = u64::MAX;
        assert!(
            broker
                .revoke(&scope("KEY"), "owner", Duration::ZERO)
                .is_err()
        );
        assert_eq!(broker.health().unwrap(), AuthorityHealth::Sealed);
        assert!(broker.admit(&AdmitRequest::secret("KEY")).is_err());
    }
}
